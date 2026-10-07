//! The session registry against local fake MCP servers (a stdlib-only
//! Python stdio server, and an in-process streamable-HTTP server), driven
//! through the same `mcp.session.*` host requests the kernel sends. Ports of
//! the in-kernel client's registry, transport, timeout and cleanup tests.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pa_types::sync::MutexExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::auth::{AuthStorage, AuthStorageData};
use crate::kernel::shared::with_host_request_cancellation;

mod http_fixture;

pub(super) fn python() -> Option<String> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        return Some(explicit.to_string_lossy().to_string());
    }
    let found = std::process::Command::new("python3")
        .arg("-c")
        .arg("import sys; print(sys.executable)")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    if found.is_none() {
        eprintln!("python3 not found; skipping the stdio MCP session test");
    }
    found
}

pub(super) fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp")
        .join(name)
}

pub(super) struct Harness {
    pub(super) sessions: McpSessions,
    handlers: HostRequestHandlers,
    configs: Arc<Mutex<HashMap<String, Value>>>,
    pub(super) dir: tempfile::TempDir,
}

impl Harness {
    pub(super) fn new() -> Self {
        Self::with_options(|_| {})
    }

    pub(super) fn with_options(adjust: impl FnOnce(&mut McpSessionOptions)) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut options = McpSessionOptions {
            cwd: dir.path().to_path_buf(),
            ..McpSessionOptions::default()
        };
        adjust(&mut options);
        Self::with_auth(dir, options, &AuthStorageData::default())
    }

    pub(super) fn with_auth(
        dir: tempfile::TempDir,
        options: McpSessionOptions,
        auth: &AuthStorageData,
    ) -> Self {
        let configs: Arc<Mutex<HashMap<String, Value>>> = Arc::default();
        let lookup = Arc::clone(&configs);
        let sessions = McpSessions::new(
            Box::new(move |server| {
                Ok(lookup
                    .lock_or_recover()
                    .get(server)
                    .cloned()
                    .unwrap_or_else(|| json!({})))
            }),
            Arc::new(tokio::sync::Mutex::new(AuthStorage::in_memory_without_env(
                auth,
                Arc::new(crate::mcp::McpOAuth::new()),
            ))),
            None,
            options,
        );
        let mut handlers = HostRequestHandlers::new();
        sessions.register_handlers(&mut handlers);
        Self {
            sessions,
            handlers,
            configs,
            dir,
        }
    }

    pub(super) fn set(&self, server: &str, config: Value) {
        self.configs
            .lock_or_recover()
            .insert(server.to_string(), config);
    }

    pub(super) fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The raw reply of one host request.
    pub(super) async fn request(&self, request_type: &str, data: Value) -> Value {
        self.request_with(request_type, data, CancellationToken::new())
            .await
    }

    pub(super) async fn request_with(
        &self,
        request_type: &str,
        data: Value,
        cancel: CancellationToken,
    ) -> Value {
        let handler = self.handlers.get(request_type).expect("registered").clone();
        let payload = HostRequestPayload {
            data,
            cell_source_code: None,
        };
        with_host_request_cancellation(cancel, handler(payload))
            .await
            .expect("session requests always answer")
    }

    pub(super) async fn call(&self, server: &str, tool: &str, arguments: Value) -> Value {
        self.request(
            "mcp.session.call_tool",
            json!({ "server": server, "tool": tool, "arguments": arguments }),
        )
        .await
    }

    /// A stdio config running the fixture server, plus `extra` keys.
    pub(super) fn stdio(python: &str, extra: Value) -> Value {
        let mut config = json!({
            "type": "stdio",
            "command": python,
            "args": [fixture("stdio_server.py").to_string_lossy(), "literal value", "$NO_SHELL"],
        });
        if let (Value::Object(config), Value::Object(extra)) = (&mut config, extra) {
            config.extend(extra);
        }
        config
    }

    pub(super) fn read_pid(&self, name: &str) -> u32 {
        std::fs::read_to_string(self.path(name))
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid")
    }

    pub(super) fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("event json"))
            .collect()
    }
}

pub(super) fn ok(value: &Value, connected: bool) -> Value {
    json!({ "ok": true, "value": value, "connected": connected })
}

pub(super) fn failed(kind: &str, message: &str, connected: bool) -> Value {
    json!({ "ok": false, "error": { "type": kind, "message": message }, "connected": connected })
}

/// Wait (bounded) for `condition` to hold.
pub(super) async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub(super) async fn process_gone(pid: u32) {
    eventually(&format!("process {pid} to exit"), || {
        !crate::platform::process::pid_exists(pid)
    })
    .await;
}

fn text_value(reply: &Value) -> Value {
    serde_json::from_str(reply["value"].as_str().expect("text result")).expect("json text")
}

#[tokio::test]
async fn a_stdio_server_gets_its_argv_cwd_and_resolved_env_and_raw_tool_names() {
    let Some(python) = python() else { return };
    let harness = Harness::with_options(|options| {
        options.kernel_env = HashMap::from([("SOURCE_VALUE".to_string(), "resolved".to_string())]);
    });
    let cwd = harness.dir.path().canonicalize().unwrap();
    harness.set(
        "svc",
        Harness::stdio(
            &python,
            json!({ "cwd": cwd, "env": { "FIXTURE_ENV": { "env": "SOURCE_VALUE" } } }),
        ),
    );
    let reply = harness
        .call("svc", "fixture/raw.tool", json!({ "x": 1 }))
        .await;
    assert_eq!(
        (reply["ok"].clone(), reply["connected"].clone()),
        (json!(true), json!(false))
    );
    let output = text_value(&reply);
    assert_eq!(
        (
            output["args"].clone(),
            Path::new(output["cwd"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            output["env"].clone(),
            output["ambient"].clone(),
            output["arguments"].clone(),
        ),
        (
            json!(["literal value", "$NO_SHELL"]),
            cwd,
            json!("resolved"),
            Value::Null,
            json!({ "x": 1 })
        )
    );
    // The second call reuses the live connection.
    let again = harness.call("svc", "fixture/raw.tool", json!({})).await;
    assert_eq!(again["connected"], json!(true));
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn a_server_without_its_own_cwd_runs_in_the_session_cwd() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    let moved = harness.path("moved");
    std::fs::create_dir(&moved).unwrap();
    harness.sessions.set_cwd(&moved);
    harness.set("svc", Harness::stdio(&python, json!({})));
    let output = text_value(&harness.call("svc", "fixture/raw.tool", json!({})).await);
    assert_eq!(
        Path::new(output["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        moved.canonicalize().unwrap()
    );
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn an_acp_stdio_server_gets_exactly_its_literal_env() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    harness.set(
        "task-tools",
        Harness::stdio(
            &python,
            json!({ "env": { "FIXTURE_ENV": "task-secret" }, "credentialSource": "acp" }),
        ),
    );
    let output = text_value(
        &harness
            .call("task-tools", "fixture/raw.tool", json!({}))
            .await,
    );
    assert_eq!(
        (output["env"].clone(), output["ambient"].clone()),
        (json!("task-secret"), Value::Null)
    );
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn policy_filters_the_listing_and_dispatch() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    harness.set(
        "svc",
        Harness::stdio(
            &python,
            json!({ "enabledTools": ["fixture/raw.tool", "denied"], "disabledTools": ["denied"] }),
        ),
    );
    let listed = harness
        .request("mcp.session.list_tools", json!({ "server": "svc" }))
        .await;
    assert_eq!(
        listed,
        ok(
            &json!([{ "name": "fixture/raw.tool", "description": "fixture/raw.tool fixture", "inputSchema": { "type": "object" } }]),
            false
        )
    );
    assert_eq!(
        harness.call("svc", "denied", json!({})).await,
        failed(
            "PermissionError",
            "MCP tool 'denied' is disabled for server 'svc'",
            true
        )
    );
    assert_eq!(
        harness
            .request(
                "mcp.session.describe_tool",
                json!({ "server": "svc", "tool": "missing" })
            )
            .await,
        failed("KeyError", "MCP server 'svc' has no tool 'missing'", true)
    );
    assert_eq!(
        harness
            .request(
                "mcp.session.describe_tool",
                json!({ "server": "svc", "tool": "other" })
            )
            .await,
        failed(
            "PermissionError",
            "MCP tool 'other' is disabled for server 'svc'",
            true
        )
    );
    assert_eq!(
        harness
            .request(
                "mcp.session.search_tools",
                json!({ "server": "svc", "query": "FIXTURE", "limit": 5 })
            )
            .await,
        ok(
            &json!([{ "connectionId": "svc", "name": "fixture/raw.tool", "description": "fixture/raw.tool fixture" }]),
            true
        )
    );
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn tool_results_parse_and_server_errors_raise_tool_errors() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    harness.set("svc", Harness::stdio(&python, json!({})));
    assert_eq!(
        harness
            .call("svc", "structured", json!({ "issues": [1, 2] }))
            .await,
        ok(&json!({ "issues": [1, 2] }), false)
    );
    assert_eq!(
        harness.call("svc", "fail", json!({})).await,
        failed("McpToolError", "redacted failure", true)
    );
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn undeclared_and_disabled_servers_fail_without_opening_anything() {
    let harness = Harness::new();
    harness.set(
        "off",
        json!({ "type": "stdio", "command": "never-run", "enabled": false }),
    );
    assert_eq!(
        (
            harness.call("nope", "tool", json!({})).await,
            harness.call("off", "tool", json!({})).await,
        ),
        (
            failed(
                "KeyError",
                "MCP server 'nope' is not declared in user settings",
                false
            ),
            failed("RuntimeError", "MCP server 'off' is disabled", false),
        )
    );
    assert_eq!(harness.sessions.open_connections().await, 0);
}

#[tokio::test]
async fn malformed_configs_are_value_errors() {
    let harness = Harness::new();
    let cases = [
        (
            json!({ "type": "stdio", "args": [] }),
            "MCP server 'svc' requires command and string args",
        ),
        (
            json!({ "type": "stdio", "command": "x", "args": [1] }),
            "MCP server 'svc' requires command and string args",
        ),
        (
            json!({ "type": "stdio", "command": "x", "cwd": 3 }),
            "MCP server 'svc' cwd must be a string",
        ),
        (
            json!({ "type": "stdio", "command": "x", "env": { "A": "literal" } }),
            "MCP stdio env values must use {\"env\": \"NAME\"} references",
        ),
        (
            json!({ "type": "stdio", "command": "x", "env": { "A": { "env": "PA_TEST_SURELY_UNSET_VAR" } } }),
            "MCP stdio environment reference for 'A' is unavailable",
        ),
        (
            json!({ "type": "sse", "url": "http://x" }),
            "MCP server 'svc' has unsupported transport 'sse'",
        ),
        (json!({ "type": "http" }), "MCP server 'svc' requires a URL"),
        (
            json!({ "type": "stdio", "command": "x", "startupTimeoutMs": true }),
            "MCP timeouts must be positive milliseconds",
        ),
        (
            json!({ "type": "stdio", "command": "x", "callTimeoutMs": 0 }),
            "MCP timeouts must be positive milliseconds",
        ),
    ];
    for (config, message) in cases {
        harness.set("svc", config.clone());
        assert_eq!(
            harness.call("svc", "tool", json!({})).await,
            failed("ValueError", message, false),
            "{config}"
        );
    }
}

#[tokio::test]
async fn a_missing_command_is_file_not_found() {
    let harness = Harness::new();
    harness.set(
        "svc",
        json!({ "type": "stdio", "command": "pa-surely-missing-mcp-server" }),
    );
    assert_eq!(
        harness.call("svc", "tool", json!({})).await,
        failed(
            "FileNotFoundError",
            "[Errno 2] No such file or directory: 'pa-surely-missing-mcp-server'",
            false
        )
    );
}

#[tokio::test]
async fn a_changed_config_closes_the_old_connection_before_opening_the_new_one() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    let with_pid = |name: &str| {
        Harness::stdio(
            &python,
            json!({ "env": { "FIXTURE_PID_FILE": harness.path(name) }, "credentialSource": "acp" }),
        )
    };
    harness.set("svc", with_pid("first.pid"));
    harness.call("svc", "fixture/raw.tool", json!({})).await;
    // The same configuration reuses the connection.
    assert_eq!(
        harness.call("svc", "fixture/raw.tool", json!({})).await["connected"],
        json!(true)
    );
    let first = harness.read_pid("first.pid");
    harness.set("svc", with_pid("second.pid"));
    let reply = harness.call("svc", "fixture/raw.tool", json!({})).await;
    assert_eq!(
        (reply["ok"].clone(), reply["connected"].clone()),
        (json!(true), json!(true))
    );
    process_gone(first).await;
    let second = harness.read_pid("second.pid");
    assert!(crate::platform::process::pid_exists(second));
    harness.sessions.close_all().await;
    process_gone(second).await;
}

#[tokio::test]
async fn a_config_failure_keeps_the_cached_connection() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    harness.set("svc", Harness::stdio(&python, json!({})));
    harness.call("svc", "fixture/raw.tool", json!({})).await;
    harness.set("svc", json!({}));
    assert_eq!(
        harness.call("svc", "fixture/raw.tool", json!({})).await,
        failed(
            "KeyError",
            "MCP server 'svc' is not declared in user settings",
            true
        )
    );
    harness.set("svc", Harness::stdio(&python, json!({})));
    assert_eq!(
        harness.call("svc", "fixture/raw.tool", json!({})).await["connected"],
        json!(true)
    );
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn reload_reaps_the_server_process_and_the_next_call_reopens() {
    let Some(python) = python() else { return };
    let harness = Harness::with_options(|_| {});
    let pid_file = harness.path("stdio.pid");
    harness.set(
        "task-tools",
        Harness::stdio(
            &python,
            json!({ "env": { "FIXTURE_PID_FILE": pid_file }, "credentialSource": "acp" }),
        ),
    );
    harness
        .request("mcp.session.list_tools", json!({ "server": "task-tools" }))
        .await;
    let pid = harness.read_pid("stdio.pid");
    assert!(crate::platform::process::pid_exists(pid));
    assert_eq!(
        harness
            .request("mcp.session.reload", json!({ "server": "task-tools" }))
            .await,
        ok(&Value::Null, false)
    );
    process_gone(pid).await;
    assert_eq!(harness.sessions.open_connections().await, 0);
    let reopened = harness
        .request("mcp.session.list_tools", json!({ "server": "task-tools" }))
        .await;
    assert_eq!(reopened["connected"], json!(false));
    assert_ne!(harness.read_pid("stdio.pid"), pid);
    harness.request("mcp.session.close", json!({})).await;
    assert_eq!(harness.sessions.open_connections().await, 0);
    // Closing again is a no-op.
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn reload_waits_for_an_in_flight_first_open() {
    let Some(python) = python() else { return };
    let harness = Arc::new(Harness::new());
    let release = harness.path("release");
    let pid_file = harness.path("held.pid");
    harness.set(
        "svc",
        Harness::stdio(
            &python,
            json!({ "env": { "FIXTURE_HOLD_INITIALIZE": release, "FIXTURE_PID_FILE": pid_file }, "credentialSource": "acp" }),
        ),
    );
    let opening = {
        let harness = Arc::clone(&harness);
        tokio::spawn(async move {
            harness
                .request("mcp.session.list_tools", json!({ "server": "svc" }))
                .await
        })
    };
    eventually("the held server to start", || pid_file.exists()).await;
    let reload = {
        let harness = Arc::clone(&harness);
        tokio::spawn(async move { harness.request("mcp.session.reload", json!({})).await })
    };
    tokio::task::yield_now().await;
    assert!(!reload.is_finished());
    std::fs::write(&release, "").unwrap();
    assert_eq!(opening.await.unwrap()["ok"], json!(true));
    reload.await.unwrap();
    assert_eq!(harness.sessions.open_connections().await, 0);
    process_gone(harness.read_pid("held.pid")).await;
}

#[tokio::test]
async fn a_failing_server_does_not_affect_another() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    harness.set(
        "bad",
        json!({ "type": "stdio", "command": python, "args": ["-c", "raise SystemExit(3)"] }),
    );
    harness.set("good", Harness::stdio(&python, json!({})));
    let bad = harness.call("bad", "tool", json!({})).await;
    assert_eq!(bad["error"]["type"], json!("McpStartupError"));
    assert_eq!(
        harness.call("good", "fixture/raw.tool", json!({})).await["ok"],
        json!(true)
    );
    assert_eq!(harness.sessions.open_connections().await, 1);
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn the_startup_diagnostic_is_redacted_bounded_and_the_child_reaped() {
    let Some(python) = python() else { return };
    let secret = "stdio-secret-SENTINEL";
    let harness = Harness::with_options(|options| {
        options.kernel_env = HashMap::from([("SOURCE_SECRET".to_string(), secret.to_string())]);
    });
    let pid_file = harness.path("child.pid");
    harness.set(
        "svc",
        json!({
            "type": "stdio",
            "command": python,
            "args": [fixture("failing_server.py"), pid_file],
            "env": { "FIXTURE_SECRET": { "env": "SOURCE_SECRET" } },
        }),
    );
    let reply = harness.call("svc", "tool", json!({})).await;
    assert_eq!(reply["error"]["type"], json!("McpStartupError"), "{reply}");
    let diagnostic = reply["error"]["message"].as_str().unwrap();
    assert!(
        diagnostic.starts_with(
            "MCP stdio server failed during startup (MCPError: Connection closed). Stderr tail:\n"
        ),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("sentinel tail [REDACTED]"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains(secret));
    assert!(!diagnostic.contains('\x1b') && !diagnostic.contains('\0'));
    assert!(diagnostic.len() <= diagnostic::STDERR_BYTE_LIMIT + 1200);
    assert!(diagnostic.lines().count() <= diagnostic::STDERR_LINE_LIMIT + 1);
    process_gone(harness.read_pid("child.pid")).await;
    assert_eq!(harness.sessions.open_connections().await, 0);
}

#[tokio::test]
async fn a_short_configured_secret_omits_all_child_output() {
    let Some(python) = python() else { return };
    let harness = Harness::with_options(|options| {
        options.kernel_env = HashMap::from([("SOURCE_SECRET".to_string(), "xy".to_string())]);
    });
    harness.set(
        "svc",
        json!({
            "type": "stdio",
            "command": python,
            "args": [fixture("failing_server.py"), harness.path("child.pid")],
            "env": { "FIXTURE_SECRET": { "env": "SOURCE_SECRET" } },
        }),
    );
    assert_eq!(
        harness.call("svc", "tool", json!({})).await,
        failed(
            "McpStartupError",
            "MCP stdio server failed during startup (MCPError: details omitted for safe redaction).",
            false
        )
    );
}

#[tokio::test]
async fn the_startup_deadline_times_out_and_reaps_the_child() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    let pid_file = harness.path("held.pid");
    harness.set(
        "svc",
        Harness::stdio(
            &python,
            json!({
                "env": { "FIXTURE_HOLD_INITIALIZE": harness.path("never"), "FIXTURE_PID_FILE": pid_file },
                "credentialSource": "acp",
                "startupTimeoutMs": 300,
            }),
        ),
    );
    assert_eq!(
        harness.call("svc", "tool", json!({})).await,
        failed("TimeoutError", "", false)
    );
    process_gone(harness.read_pid("held.pid")).await;
}

#[tokio::test]
async fn the_call_deadline_cancels_the_request_at_the_server() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    let events = harness.path("events.jsonl");
    harness.set(
        "svc",
        Harness::stdio(
            &python,
            json!({ "env": { "FIXTURE_EVENTS_FILE": events }, "credentialSource": "acp", "callTimeoutMs": 200 }),
        ),
    );
    assert_eq!(
        harness.call("svc", "slow", json!({})).await,
        failed("TimeoutError", "", false)
    );
    eventually("the server to see the cancellation", || {
        harness
            .events()
            .iter()
            .any(|event| event["method"] == "notifications/cancelled")
    })
    .await;
    // The connection stays usable.
    assert_eq!(
        harness.call("svc", "fixture/raw.tool", json!({})).await["connected"],
        json!(true)
    );
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn a_host_cancel_abandons_the_call_and_tells_the_server() {
    let Some(python) = python() else { return };
    let harness = Arc::new(Harness::new());
    let events = harness.path("events.jsonl");
    harness.set(
        "svc",
        Harness::stdio(
            &python,
            json!({ "env": { "FIXTURE_EVENTS_FILE": events }, "credentialSource": "acp" }),
        ),
    );
    harness.call("svc", "fixture/raw.tool", json!({})).await;
    let cancel = CancellationToken::new();
    let call = {
        let harness = Arc::clone(&harness);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            harness
                .request_with(
                    "mcp.session.call_tool",
                    json!({ "server": "svc", "tool": "slow" }),
                    cancel,
                )
                .await
        })
    };
    eventually("the server to receive the call", || {
        harness
            .events()
            .iter()
            .any(|event| event["method"] == "tools/call" && event["name"] == "slow")
    })
    .await;
    cancel.cancel();
    assert_eq!(call.await.unwrap(), failed("CancelledError", "", true));
    eventually("the server to see the cancellation", || {
        harness
            .events()
            .iter()
            .any(|event| event["method"] == "notifications/cancelled")
    })
    .await;
    assert_eq!(
        harness.call("svc", "fixture/raw.tool", json!({})).await["connected"],
        json!(true)
    );
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn a_cancel_during_startup_reaps_the_child() {
    let Some(python) = python() else { return };
    let harness = Arc::new(Harness::new());
    let pid_file = harness.path("held.pid");
    harness.set(
        "svc",
        Harness::stdio(
            &python,
            json!({ "env": { "FIXTURE_HOLD_INITIALIZE": harness.path("never"), "FIXTURE_PID_FILE": pid_file }, "credentialSource": "acp" }),
        ),
    );
    let cancel = CancellationToken::new();
    let opening = {
        let harness = Arc::clone(&harness);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            harness
                .request_with("mcp.session.list_tools", json!({ "server": "svc" }), cancel)
                .await
        })
    };
    eventually("the held server to start", || pid_file.exists()).await;
    cancel.cancel();
    assert_eq!(opening.await.unwrap(), failed("CancelledError", "", false));
    process_gone(harness.read_pid("held.pid")).await;
    assert_eq!(harness.sessions.open_connections().await, 0);
}

#[tokio::test]
async fn a_dead_server_reopens_on_next_use() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    let pid_file = harness.path("stdio.pid");
    harness.set(
        "svc",
        Harness::stdio(
            &python,
            json!({ "env": { "FIXTURE_PID_FILE": pid_file }, "credentialSource": "acp" }),
        ),
    );
    harness.call("svc", "fixture/raw.tool", json!({})).await;
    let pid = harness.read_pid("stdio.pid");
    assert!(crate::platform::process::kill_process_group_or_pid(
        pid as i32
    ));
    // Dead (a zombie until the session reaps it).
    eventually("the killed server to exit", || {
        let stat = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_default();
        stat.is_empty() || stat.starts_with('Z')
    })
    .await;
    let reply = harness.call("svc", "fixture/raw.tool", json!({})).await;
    assert_eq!(
        (reply["ok"].clone(), reply["connected"].clone()),
        (json!(true), json!(false))
    );
    assert_ne!(harness.read_pid("stdio.pid"), pid);
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn idle_connections_are_reaped() {
    let Some(python) = python() else { return };
    let harness = Harness::with_options(|options| {
        options.idle_timeout = Some(Duration::from_millis(100));
    });
    let pid_file = harness.path("stdio.pid");
    harness.set(
        "svc",
        Harness::stdio(
            &python,
            json!({ "env": { "FIXTURE_PID_FILE": pid_file }, "credentialSource": "acp" }),
        ),
    );
    harness.call("svc", "fixture/raw.tool", json!({})).await;
    let pid = harness.read_pid("stdio.pid");
    process_gone(pid).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness.sessions.open_connections().await != 0 {
        assert!(
            Instant::now() < deadline,
            "the idle connection never closed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn closing_runs_concurrently_under_one_bound_and_kills_stubborn_trees() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    for name in ["one", "two"] {
        let pid_file = harness.path(&format!("{name}.pid"));
        harness.set(
            name,
            Harness::stdio(
                &python,
                json!({ "env": { "FIXTURE_PID_FILE": pid_file, "FIXTURE_IGNORE_EOF": "1" }, "credentialSource": "acp" }),
            ),
        );
        harness.call(name, "fixture/raw.tool", json!({})).await;
    }
    let started = Instant::now();
    harness.sessions.close_all().await;
    // Both graceful waits ran at once: well under two serial bounds.
    assert!(
        started.elapsed() < generation::SHUTDOWN_TIMEOUT * 2,
        "{:?}",
        started.elapsed()
    );
    for name in ["one", "two"] {
        process_gone(harness.read_pid(&format!("{name}.pid"))).await;
    }
}

#[tokio::test]
async fn dropping_the_sessions_kills_their_servers() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    let pid_file = harness.path("stdio.pid");
    harness.set(
        "svc",
        Harness::stdio(
            &python,
            json!({ "env": { "FIXTURE_PID_FILE": pid_file, "FIXTURE_IGNORE_EOF": "1" }, "credentialSource": "acp" }),
        ),
    );
    harness.call("svc", "fixture/raw.tool", json!({})).await;
    let pid = harness.read_pid("stdio.pid");
    let Harness {
        sessions,
        handlers,
        dir,
        ..
    } = harness;
    drop(handlers);
    drop(sessions);
    process_gone(pid).await;
    drop(dir);
}

#[tokio::test]
async fn pagination_is_followed_and_dishonest_cursors_refuse() {
    let Some(python) = python() else { return };
    let harness = Harness::new();
    let pages = |pages: Value| {
        Harness::stdio(
            &python,
            json!({ "env": { "FIXTURE_TOOLS_PAGES": pages.to_string() }, "credentialSource": "acp" }),
        )
    };
    harness.set(
        "paged",
        pages(json!([[["one"], "cursor-1"], [["two", "one"], null]])),
    );
    let listed = harness
        .request("mcp.session.list_tools", json!({ "server": "paged" }))
        .await;
    let names: Vec<&str> = listed["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["one", "two"]);
    harness.set("repeated", pages(json!([[["one"], "same"]])));
    let repeated = harness
        .request("mcp.session.list_tools", json!({ "server": "repeated" }))
        .await;
    // A stdio server's inventory failure is a startup failure.
    assert_eq!(
        repeated,
        failed(
            "McpStartupError",
            "MCP stdio server failed during startup (McpDiscoveryError: MCP server 'repeated' \
             repeated a tools/list pagination cursor; its tool inventory cannot be completed).",
            false
        )
    );
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn http_discovery_errors_are_not_wrapped() {
    let server = http_fixture::FakeHttpServer::start(http_fixture::Behavior {
        tool_pages: vec![(vec!["one".to_string()], Some(String::new()))],
        ..http_fixture::Behavior::default()
    })
    .await;
    let harness = Harness::new();
    harness.set("svc", json!({ "type": "http", "url": server.url() }));
    assert_eq!(
        harness
            .request("mcp.session.list_tools", json!({ "server": "svc" }))
            .await,
        failed(
            "McpDiscoveryError",
            "MCP server 'svc' returned a malformed tools/list pagination cursor",
            false
        )
    );
}

#[tokio::test]
async fn an_anonymous_streamable_http_server_lists_and_calls() {
    let server = http_fixture::FakeHttpServer::start(http_fixture::Behavior::default()).await;
    let harness = Harness::new();
    harness.set(
        "svc",
        json!({ "type": "http", "url": server.url(), "headers": { "X-Team": "eng" } }),
    );
    let listed = harness
        .request("mcp.session.list_tools", json!({ "server": "svc" }))
        .await;
    assert_eq!(
        listed,
        ok(
            &json!([{ "name": "http/raw.tool", "description": "", "inputSchema": { "type": "object" } }]),
            false
        )
    );
    assert_eq!(
        harness
            .call("svc", "http/raw.tool", json!({ "value": "ok" }))
            .await,
        ok(&json!({ "value": "ok" }), true)
    );
    let requests = server.requests();
    assert!(requests
        .iter()
        .all(|request| request.header("x-team") == Some("eng")));
    assert!(requests
        .iter()
        .all(|request| request.header("authorization").is_none()));
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn http_headers_carry_the_bearer_env_token_last() {
    let server = http_fixture::FakeHttpServer::start(http_fixture::Behavior::default()).await;
    let harness = Harness::with_options(|options| {
        options.kernel_env = HashMap::from([("SVC_TOKEN".to_string(), " env-secret ".to_string())]);
    });
    harness.set(
        "svc",
        json!({
            "type": "http",
            "url": server.url(),
            "headers": { "Authorization": "Bearer configured" },
            "bearerTokenEnvVar": "SVC_TOKEN",
        }),
    );
    assert_eq!(
        harness
            .call("svc", "http/raw.tool", json!({ "value": 1 }))
            .await["ok"],
        json!(true)
    );
    assert!(server
        .requests()
        .iter()
        .all(|request| request.header("authorization") == Some("Bearer env-secret")));
    harness.sessions.close_all().await;
}

#[tokio::test]
async fn a_redirecting_http_endpoint_never_receives_a_followed_request() {
    let target = http_fixture::FakeHttpServer::start(http_fixture::Behavior::default()).await;
    let redirecting = http_fixture::FakeHttpServer::start(http_fixture::Behavior {
        redirect_to: Some(target.url()),
        ..http_fixture::Behavior::default()
    })
    .await;
    let harness = Harness::with_options(|options| {
        options.kernel_env = HashMap::from([("SVC_TOKEN".to_string(), "secret".to_string())]);
    });
    harness.set(
        "svc",
        json!({ "type": "http", "url": redirecting.url(), "bearerTokenEnvVar": "SVC_TOKEN" }),
    );
    let reply = harness.call("svc", "http/raw.tool", json!({})).await;
    assert_eq!(reply["error"]["type"], json!("RuntimeError"), "{reply}");
    assert!(!reply["error"]["message"]
        .as_str()
        .unwrap()
        .contains("secret"));
    assert_eq!(target.requests().len(), 0);
}

#[tokio::test]
async fn an_integration_call_is_a_per_call_connection_with_its_headers() {
    let server = http_fixture::FakeHttpServer::start(http_fixture::Behavior::default()).await;
    let harness = Harness::new();
    let connection = json!({
        "server": "demo",
        "url": server.url(),
        "headers": { "Authorization": "Bearer tok-xyz" },
    });
    let listed = harness
        .request("mcp.integration.list_tools", connection.clone())
        .await;
    assert_eq!(listed["value"][0]["name"], json!("http/raw.tool"));
    let mut call = connection.clone();
    call["tool"] = json!("http/raw.tool");
    call["arguments"] = json!({ "value": "x" });
    assert_eq!(
        harness.request("mcp.integration.call_tool", call).await,
        ok(&json!({ "value": "x" }), false)
    );
    assert!(server
        .requests()
        .iter()
        .all(|request| request.header("authorization") == Some("Bearer tok-xyz")));
    // Two connections, each initialized once and closed.
    assert_eq!(server.initializations(), 2);
    assert_eq!(harness.sessions.open_connections().await, 0);
}
