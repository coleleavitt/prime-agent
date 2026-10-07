//! A `host_cancel` for a `bash.run` the host is still waiting on (the kernel
//! was interrupted inside `bash()`) kills the run's command and the run still
//! settles: its reply carries the killed result and the reap, once the
//! command's process group is gone. A fake runtime sends the run (a command
//! that would sleep for 30 s), cancels it, and echoes the terminal reply.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions, HostRequestHandlers, KernelManagerOptions, KernelShutdownOptions,
};
use serde_json::{json, Value};

/// Speaks protocol v5: ready, then per cell one `bash.run` host request
/// (the cell code is the command), a `host_cancel` for it 300 ms later, and
/// the terminal `host_reply` as the cell's stdout.
const FAKE_RUNTIME: &str = r#"#!/usr/bin/env python3
import json, os, sys, threading

def send(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()

send({"event": "ready", "protocol": 5, "python": "3.13.0"})
pending = None
for line in sys.stdin:
    try:
        frame = json.loads(line)
    except Exception:
        continue
    if frame.get("type") == "host_reply":
        if pending is not None and frame.get("id") == "h-" + pending:
            send({"event": "stdout", "id": pending, "text": json.dumps(frame.get("data"))})
            send({"event": "done", "id": pending, "status": "ok"})
            pending = None
        continue
    code = frame.get("code")
    cell = frame.get("id")
    if not isinstance(code, str) or not code:
        send({"event": "done", "id": cell, "status": "ok"})
        continue
    pending = cell
    send({"event": "host_request", "id": "h-" + cell, "data": {
        "type": "bash.run", "command": code, "script": code, "cwd": os.getcwd(),
        "env": {"PATH": "/usr/bin:/bin"}, "launchBypass": [], "allow": [],
        "kernelPid": os.getpid(), "waitMs": 20000,
    }})
    threading.Timer(0.3, send, ({"event": "host_cancel", "id": "h-" + cell},)).start()
"#;

fn group_alive(pgid: i64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", "--", &format!("-{pgid}")])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[tokio::test]
async fn a_cancelled_run_kills_its_command_and_settles() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let runtime = dir.path().join("fake-kernel");
    std::fs::write(&runtime, FAKE_RUNTIME).expect("write fake runtime");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let manager = ReplKernelManager::new(KernelManagerOptions {
        sandbox: None,
        environment: pa_core::kernel::shared::KernelEnvironment::Inherit,
        plan_guard: None,
        python: Some(runtime),
        cwd: Some(dir.path().to_path_buf()),
        env: HashMap::new(),
        session_id: Some("bash-run-cancel-test".to_string()),
        host_handlers: HostRequestHandlers::new(),
        python_skills: Vec::new(),
        snapshot: None,
        bootstrap_code: None,
        stderr_log_path: None,
        on_background_work_settled: None,
    });
    manager
        .start(KernelStartOptions::default())
        .await
        .expect("fake kernel must start");

    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        manager.execute("sleep 30", ExecuteOptions::default()),
    )
    .await
    .expect("the cancelled run settles")
    .expect("execute");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    let reply: Value = serde_json::from_str(result.stdout.trim()).expect("reply json");
    assert_eq!(reply["status"], "ok", "{reply}");
    let run = &reply["result"];
    assert_eq!(run["status"], "ok", "{run}");
    assert_eq!(run["cancelled"], true);
    assert_eq!(run["done"], true);
    let events: Vec<Value> = run["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| json!({"type": event["type"], "exitCode": event.get("exitCode")}))
        .collect();
    assert_eq!(
        events,
        vec![
            json!({"type": "finished", "exitCode": -15}),
            json!({"type": "reaped", "exitCode": null}),
        ]
    );
    let pgid = run["job"]["pgid"].as_i64().expect("pgid");
    assert!(
        !group_alive(pgid),
        "the command's group outlived the cancel"
    );

    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        manager.shutdown(KernelShutdownOptions::default()),
    )
    .await;
}
