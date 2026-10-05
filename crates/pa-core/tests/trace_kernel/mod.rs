//! A fake kernel runtime that reports the trace-context carriers it saw:
//! the `TRACEPARENT` it inherited and the `traceparent` field of the request
//! frame, printed as one JSON stdout line per execute.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions, HostRequestHandlers, KernelManagerOptions, KernelShutdownOptions,
};

const FAKE_RUNTIME: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

print(json.dumps({"event": "ready", "protocol": 4, "python": "3.13.0"}), flush=True)
for line in sys.stdin:
    try:
        req = json.loads(line)
    except Exception:
        continue
    if req.get("type") == "shutdown":
        break
    seen = {"frame": req.get("traceparent"), "env": os.environ.get("TRACEPARENT")}
    sys.stdout.write(json.dumps({"event": "stdout", "id": req.get("id"), "text": json.dumps(seen)}) + "\n")
    sys.stdout.write(json.dumps({"event": "done", "id": req.get("id"), "status": "ok"}) + "\n")
    sys.stdout.flush()
"#;

/// The carriers one execute request observed.
#[derive(Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Carriers {
    pub frame: Option<String>,
    pub env: Option<String>,
}

/// Start the fake kernel, run one cell, and return what the runtime saw.
pub async fn observe_carriers() -> Carriers {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let python = dir.path().join("fake-kernel");
    std::fs::write(&python, FAKE_RUNTIME).expect("write fake runtime");
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let manager = ReplKernelManager::new(KernelManagerOptions {
        environment: pa_core::kernel::shared::KernelEnvironment::Inherit,
        python: Some(python),
        cwd: Some(std::env::temp_dir()),
        env: HashMap::new(),
        session_id: Some("trace-context-test".to_string()),
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
    let result = manager
        .execute("cell", ExecuteOptions::default())
        .await
        .expect("execute must not fail");
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        manager.shutdown(KernelShutdownOptions::default()),
    )
    .await;
    serde_json::from_str(result.stdout.trim()).expect("the fake reports its carriers")
}
