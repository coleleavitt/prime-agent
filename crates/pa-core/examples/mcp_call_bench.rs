// Same bounded-boundary disposition as src/lib.rs: large_futures stack
// futures by design.
#![allow(clippy::large_futures)]

//! `rlm.mcp.call_tool` round-trip benchmark (verifier tooling, not a test):
//! a real `python -m rlm.repl` kernel calls a local fake stdio MCP server
//! declared in `mcpServers`, through the session's own `mcp.*` host
//! handlers. The cell times each `await mcp.call_tool(...)` itself, so the
//! numbers are what model-authored code observes. The first call (which
//! opens the connection) is reported separately.
//!
//! Usage: `cargo run -p pa-core --example mcp_call_bench` (kernel Python from
//! `PA_CORE_KERNEL_PYTHON` or the bootstrapped kernel venv; `PA_BENCH_ROUNDS`
//! calls, default 200).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions,
    ExecuteStatus,
    HostRequestHandlers,
    KernelManagerOptions,
    KernelShutdownOptions,
};
use pa_core::mcp::{McpManager, McpManagerOptions, McpOAuth, McpServerConfig, McpSessionOptions};

/// A minimal stdio MCP server: initialize, one `echo` tool, echoed calls.
const FAKE_SERVER: &str = r#"import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if request.get("id") is None:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "bench", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "echo", "inputSchema": {"type": "object"}}]}
    else:
        result = {"content": [{"type": "text", "text": json.dumps(request["params"].get("arguments", {}))}]}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#;

fn kernel_python() -> PathBuf {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        return PathBuf::from(explicit);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(format!("{home}/.prime/agent/kernel-venv/bin/python"))
}

#[tokio::main]
async fn main() {
    let rounds: u32 = std::env::var("PA_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    let dir = tempfile::TempDir::new().expect("temp dir");
    let server = dir.path().join("bench_server.py");
    std::fs::write(&server, FAKE_SERVER).expect("write fake server");
    let python = kernel_python();
    let servers = HashMap::from([(
        "bench".to_string(),
        McpServerConfig::Stdio {
            command: python.to_string_lossy().to_string(),
            args: Some(vec![server.to_string_lossy().to_string()]),
            cwd: None,
            env: None,
            enabled: None,
            enabled_tools: None,
            disabled_tools: None,
            startup_timeout_ms: None,
            call_timeout_ms: None,
        },
    )]);
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    let manager = Arc::new(Mutex::new(McpManager::new(McpManagerOptions {
        auth_storage: pa_core::auth::AuthStorage::create_with_oauth(
            &agent_dir,
            Arc::new(McpOAuth::new()),
        ),
        get_user_servers: Box::new(move || Some(servers.clone())),
        begin_login: None,
        agent_dir: None,
        get_catalog_sources: None,
        remote_source: None,
        probe_override: None,
    })));
    let mut handlers = HostRequestHandlers::new();
    McpManager::register_host_handlers(&manager, &mut handlers);
    let sessions = McpManager::register_session_handlers(
        &manager,
        &mut handlers,
        McpSessionOptions {
            cwd: dir.path().to_path_buf(),
            ..McpSessionOptions::default()
        },
    );
    let kernel = ReplKernelManager::new(KernelManagerOptions {
        python: Some(python),
        cwd: Some(dir.path().to_path_buf()),
        session_id: Some("mcp-call-bench".to_string()),
        host_handlers: handlers,
        ..KernelManagerOptions::default()
    });
    kernel
        .start(KernelStartOptions::default())
        .await
        .expect("kernel start");
    let cell = format!(
        "import time, statistics\n\
         import rlm.mcp as mcp\n\
         _t = time.perf_counter(); await mcp.call_tool('bench', 'echo', {{'i': -1}}); _first = (time.perf_counter() - _t) * 1000\n\
         _samples = []\n\
         for _i in range({rounds}):\n\
         \x20   _t = time.perf_counter(); await mcp.call_tool('bench', 'echo', {{'i': _i}}); _samples.append((time.perf_counter() - _t) * 1000)\n\
         _samples.sort()\n\
         print(f'first-call-ms {{_first:.2f}}')\n\
         print(f'rounds {rounds} median-ms {{statistics.median(_samples):.3f}} p95-ms {{_samples[int(len(_samples) * 0.95) - 1]:.3f}} mean-ms {{statistics.mean(_samples):.3f}}')\n"
    );
    let result = kernel
        .execute(&cell, ExecuteOptions::default())
        .await
        .expect("bench cell");
    print!("{}", result.stdout);
    assert_eq!(result.status, ExecuteStatus::Ok, "{result:?}");
    let _ = kernel
        .shutdown(KernelShutdownOptions {
            snapshot: false,
            drain_host_requests: false,
        })
        .await;
    sessions.close_all().await;
}
