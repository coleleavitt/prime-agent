//! Verifier integration test: a runtime `host_cancel` frame reaches the
//! cancellation token of the host-request handler it names. A fake runtime
//! issues one host request per cell, optionally cancels it at once, then
//! echoes the terminal `host_reply` it receives as the cell's stdout.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions,
    HostRequestHandlers,
    KernelManagerOptions,
    KernelShutdownOptions,
    host_handler,
    host_request_cancellation,
};
use serde_json::{Value, json};

/// Speaks protocol v4: ready, then per cell one `host_request` (id `h-<cell
/// id>`, payload type = the cell code), a `host_cancel` for it when the code
/// is `probe.wait`, and — once the matching `host_reply` arrives on stdin —
/// the reply's data as one stdout frame plus `done`.
const FAKE_RUNTIME: &str = r#"#!/usr/bin/env python3
import json
import sys

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
    if not isinstance(code, str) or not code.startswith("probe."):
        send({"event": "done", "id": cell, "status": "ok"})
        continue
    pending = cell
    send({"event": "host_request", "id": "h-" + cell, "data": {"type": code}})
    if code == "probe.wait":
        send({"event": "host_cancel", "id": "h-" + cell})
"#;

fn fake_runtime_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("fake-kernel");
    std::fs::write(&path, FAKE_RUNTIME).expect("write fake runtime");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// `probe.wait` settles only once its token fires; `probe.peek` reports the
/// token's state without waiting.
fn handlers() -> HostRequestHandlers {
    let mut handlers = HostRequestHandlers::new();
    handlers.register(
        "probe.wait",
        host_handler(|_| async {
            let token = host_request_cancellation().expect("a handler runs with a token");
            token.cancelled().await;
            Ok(json!({ "cancelled": true }))
        }),
    );
    handlers.register(
        "probe.peek",
        host_handler(|_| async {
            let token = host_request_cancellation().expect("a handler runs with a token");
            Ok(json!({ "cancelled": token.is_cancelled() }))
        }),
    );
    handlers
}

async fn run_cell(manager: &ReplKernelManager, code: &str) -> Value {
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        manager.execute(code, ExecuteOptions::default()),
    )
    .await
    .unwrap_or_else(|_| panic!("cell {code} never settled"))
    .expect("execute");
    serde_json::from_str(result.stdout.trim()).expect("reply json on stdout")
}

#[tokio::test]
async fn host_cancel_fires_the_named_request_token_and_the_reply_still_settles() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let manager = ReplKernelManager::new(KernelManagerOptions {
        sandbox: None,
        environment: pa_core::kernel::shared::KernelEnvironment::Inherit,
        plan_guard: None,
        python: Some(fake_runtime_path(&dir)),
        cwd: Some(dir.path().to_path_buf()),
        env: HashMap::new(),
        session_id: Some("host-cancel-test".to_string()),
        host_handlers: handlers(),
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

    assert_eq!(
        run_cell(&manager, "probe.wait").await,
        json!({ "status": "ok", "result": { "cancelled": true } })
    );
    // A request nobody cancelled runs with a live token.
    assert_eq!(
        run_cell(&manager, "probe.peek").await,
        json!({ "status": "ok", "result": { "cancelled": false } })
    );

    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        manager.shutdown(KernelShutdownOptions::default()),
    )
    .await;
}
