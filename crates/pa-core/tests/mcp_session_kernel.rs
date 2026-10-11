// Pedantic-gate dispositions as src/lib.rs (large_futures).
#![allow(clippy::large_futures)]
// Drives a real kernel and POSIX stdio server processes.
#![cfg(unix)]

//! Verifier: the kernel's `rlm.mcp` is a thin client over the host-owned MCP
//! sessions. A real `python -m rlm.repl` kernel calls a local fake stdio MCP
//! server through the session's `mcp.*` host requests; the connection
//! outlives a kernel restart, and a kernel interrupt cancels the in-flight
//! host call at the server. The kernel Python is ambient product state;
//! skipped when absent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pa_core::kernel::cancellation::AbortSignal;
use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions,
    ExecuteStatus,
    HostRequestHandlers,
    KernelManagerOptions,
    KernelShutdownOptions,
};
use pa_core::mcp::{
    EnvRef,
    McpManager,
    McpManagerOptions,
    McpOAuth,
    McpServerConfig,
    McpSessionOptions,
    McpSessions,
};
use serde_json::{Value, json};

fn kernel_python() -> Option<PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp/stdio_server.py")
}

struct Setup {
    dir: tempfile::TempDir,
    python: PathBuf,
    handlers: HostRequestHandlers,
    sessions: McpSessions,
}

impl Setup {
    fn new(python: PathBuf) -> Self {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let env = HashMap::from([
            ("FIXTURE_PID_FILE".to_string(), env_ref("MCP_PID_FILE")),
            (
                "FIXTURE_EVENTS_FILE".to_string(),
                env_ref("MCP_EVENTS_FILE"),
            ),
        ]);
        let servers = HashMap::from([(
            "svc".to_string(),
            McpServerConfig::Stdio {
                command: python.to_string_lossy().to_string(),
                args: Some(vec![fixture().to_string_lossy().to_string()]),
                cwd: None,
                env: Some(env),
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
                kernel_env: HashMap::from([
                    (
                        "MCP_PID_FILE".to_string(),
                        dir.path().join("server.pid").to_string_lossy().to_string(),
                    ),
                    (
                        "MCP_EVENTS_FILE".to_string(),
                        dir.path()
                            .join("events.jsonl")
                            .to_string_lossy()
                            .to_string(),
                    ),
                ]),
                ..McpSessionOptions::default()
            },
        );
        Self {
            dir,
            python,
            handlers,
            sessions,
        }
    }

    async fn kernel(&self) -> ReplKernelManager {
        let kernel = ReplKernelManager::new(KernelManagerOptions {
            python: Some(self.python.clone()),
            cwd: Some(self.dir.path().to_path_buf()),
            session_id: Some("mcp-session-kernel".to_string()),
            host_handlers: self.handlers.clone(),
            ..KernelManagerOptions::default()
        });
        kernel
            .start(KernelStartOptions::default())
            .await
            .expect("kernel start");
        kernel
    }

    fn pid(&self) -> u32 {
        std::fs::read_to_string(self.dir.path().join("server.pid"))
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid")
    }

    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("event"))
            .collect()
    }
}

fn env_ref(name: &str) -> EnvRef {
    EnvRef {
        env: Some(name.to_string()),
    }
}

async fn run(kernel: &ReplKernelManager, code: &str) -> String {
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        kernel.execute(code, ExecuteOptions::default()),
    )
    .await
    .expect("cell settles")
    .expect("execute");
    assert_eq!(result.status, ExecuteStatus::Ok, "{result:?}");
    result.stdout.trim().to_string()
}

async fn shutdown(kernel: ReplKernelManager) {
    let _ = kernel
        .shutdown(KernelShutdownOptions {
            snapshot: false,
            drain_host_requests: false,
        })
        .await;
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const CALL_AND_REPORT: &str = "import json\nimport rlm.mcp as mcp\n\
    _out = json.loads(await mcp.call_tool('svc', 'fixture/raw.tool', {'x': 1}))\n\
    print(json.dumps({'arguments': _out['arguments'], 'tools': [t['name'] for t in await mcp.list_tools('svc')]}))";

#[tokio::test]
async fn rlm_mcp_calls_reach_the_host_session_and_survive_a_kernel_restart() {
    let Some(python) = kernel_python() else {
        return;
    };
    let setup = Setup::new(python);
    let first = setup.kernel().await;
    let expected = json!({
        "arguments": { "x": 1 },
        "tools": ["fixture/raw.tool", "slow", "denied", "other", "fail", "structured"],
    });
    let report = |stdout: String| serde_json::from_str::<Value>(&stdout).expect("json report");
    assert_eq!(report(run(&first, CALL_AND_REPORT).await), expected);
    // Typed failures keep their kernel-side classes.
    assert_eq!(
        run(
            &first,
            "try:\n    await mcp.call_tool('svc', 'fail')\nexcept mcp.McpToolError as exc:\n    print(type(exc).__name__, exc)\n\
             try:\n    await mcp.describe_tool('svc', 'missing')\nexcept KeyError as exc:\n    print(type(exc).__name__, exc)"
        )
        .await,
        "McpToolError redacted failure\nKeyError \"MCP server 'svc' has no tool 'missing'\""
    );
    let server = setup.pid();
    shutdown(first).await;
    // The kernel is gone; the host's connection (and its server) is not.
    assert!(pa_core::platform::process::pid_exists(server));
    let second = setup.kernel().await;
    assert_eq!(report(run(&second, CALL_AND_REPORT).await), expected);
    assert_eq!(setup.pid(), server);
    assert_eq!(setup.sessions.open_connections().await, 1);
    shutdown(second).await;
    setup.sessions.close_all().await;
    eventually("the server to exit", || {
        !pa_core::platform::process::pid_exists(server)
    })
    .await;
}

#[tokio::test]
async fn a_kernel_interrupt_cancels_the_in_flight_host_call() {
    let Some(python) = kernel_python() else {
        return;
    };
    let setup = Setup::new(python);
    let kernel = setup.kernel().await;
    let signal = AbortSignal::new();
    let cell = kernel.execute(
        "import rlm.mcp as mcp\nawait mcp.call_tool('svc', 'slow')",
        ExecuteOptions {
            signal: Some(signal.clone()),
            ..ExecuteOptions::default()
        },
    );
    let abort = async {
        eventually("the server to receive the call", || {
            setup
                .events()
                .iter()
                .any(|event| event["method"] == "tools/call" && event["name"] == "slow")
        })
        .await;
        signal.abort();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(60), async { tokio::join!(cell, abort) })
            .await
            .expect("the interrupted cell settles");
    let result = result.expect("execute");
    assert_ne!(result.status, ExecuteStatus::Ok, "{result:?}");
    eventually("the server to see the cancellation", || {
        setup
            .events()
            .iter()
            .any(|event| event["method"] == "notifications/cancelled")
    })
    .await;
    // The connection is still live for the next call.
    assert_eq!(
        run(
            &kernel,
            "print((await mcp.call_tool('svc', 'structured', {'ok': True}))['ok'])"
        )
        .await,
        "True"
    );
    shutdown(kernel).await;
    setup.sessions.close_all().await;
}
