//! The vertical slice: a real agent turn (pa-agent's loop with the scripted
//! provider) whose tool runs a cell on a kernel (pa-core's manager over a
//! fake runtime that, like `rlm.repl`, adopts the request's `traceparent`
//! and reports its `kernel.cell` span as a trace event). The recorder writes
//! `agent.jsonl`, and `prime-agent trace <id>` reconstructs the turn's
//! tree: agent.turn > llm.request, tool.execute > kernel.start,
//! kernel.execute > kernel.cell.

#![cfg(unix)]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use pa_agent::abort::AbortSignal;
use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback, Model, UsageCost};
use pa_core::kernel::manager::ReplKernelManager;
use pa_core::kernel::shared::{
    ExecuteOptions, HostRequestHandlers, KernelManagerOptions, KernelShutdownOptions,
};
use pa_trace::{RecorderConfig, RecorderHandle};
use regex::Regex;
use serde_json::Value;
use tracing_subscriber::layer::SubscriberExt;

/// Speaks protocol 5 like `rlm.repl`: each execute runs under the request's
/// `traceparent` and reports a `kernel.cell` `span_end` trace event before
/// `done`.
const FAKE_RUNTIME: &str = r#"#!/usr/bin/env python3
import json
import secrets
import sys

print(json.dumps({"event": "ready", "protocol": 5, "python": "3.13.0"}), flush=True)
for line in sys.stdin:
    try:
        req = json.loads(line)
    except Exception:
        continue
    if req.get("type") == "shutdown":
        break
    rid = req.get("id")
    parts = (req.get("traceparent") or "").split("-")
    if len(parts) == 4:
        span = {"event": "trace", "id": rid, "msg": "span_end", "name": "kernel.cell",
                "traceId": parts[1], "spanId": secrets.token_hex(8), "parentSpanId": parts[2],
                "durationMs": 1.5, "status": "error",
                "attrs": {"kernel.request_id": rid, "error": "ZeroDivisionError: division by zero"}}
        sys.stdout.write(json.dumps(span) + "\n")
    sys.stdout.write(json.dumps({"event": "stdout", "id": rid, "text": "ran"}) + "\n")
    sys.stdout.write(json.dumps({"event": "done", "id": rid, "status": "ok"}) + "\n")
    sys.stdout.flush()
"#;

fn model() -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: String::new(),
        reasoning: false,
        cost: UsageCost::default(),
        context_window: 100_000,
        max_tokens: 4_096,
    }
}

/// Runs its `code` argument as one kernel cell.
struct CellTool {
    kernel: Arc<ReplKernelManager>,
}

impl AgentTool for CellTool {
    fn name(&self) -> &'static str {
        "ipython"
    }

    fn description(&self) -> &'static str {
        "Runs a cell."
    }

    fn parameters(&self) -> &Value {
        static SCHEMA: OnceLock<Value> = OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"],
            })
        })
    }

    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let code = params["code"].as_str().unwrap_or_default().to_string();
            let result = self
                .kernel
                .execute(&code, ExecuteOptions::default())
                .await?;
            Ok(AgentToolResult::text(result.stdout))
        })
    }
}

fn fake_kernel(dir: &Path) -> Arc<ReplKernelManager> {
    let python = dir.join("fake-kernel");
    std::fs::write(&python, FAKE_RUNTIME).expect("write fake runtime");
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    Arc::new(ReplKernelManager::new(KernelManagerOptions {
        sandbox: None,
        environment: pa_core::kernel::shared::KernelEnvironment::Inherit,
        plan_guard: None,
        python: Some(python),
        cwd: Some(dir.to_path_buf()),
        env: HashMap::new(),
        session_id: Some("trace-slice".to_string()),
        host_handlers: HostRequestHandlers::new(),
        python_skills: Vec::new(),
        snapshot: None,
        bootstrap_code: None,
        stderr_log_path: None,
        on_background_work_settled: None,
    }))
}

/// Run one prompt (a tool-call turn, then a text turn) under the recorder.
fn record_a_turn(log_path: &Path, work_dir: &Path) -> RecorderHandle {
    let (layer, handle) = pa_trace::recorder(RecorderConfig {
        log_path: log_path.to_path_buf(),
        inbound: None,
        otlp: None,
    });
    pa_trace::install_context_source();
    let kernel = fake_kernel(work_dir);
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let provider = Arc::new(ScriptedProvider::new(model()));
            provider.push_tool_call_turn(
                None,
                vec![("call-1", "ipython", serde_json::json!({ "code": "1/0" }))],
            );
            provider.push_text_turn("done");
            let agent = Agent::new(AgentOptions {
                initial_state: AgentInitialState {
                    tools: Some(vec![Arc::new(CellTool {
                        kernel: Arc::clone(&kernel),
                    })]),
                    ..Default::default()
                },
                stream_fn: Some(provider.stream_fn()),
                ..Default::default()
            });
            agent.set_model(model()).await;
            agent.prompt("run it").await.expect("prompt");
            agent.wait_for_idle().await;
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                kernel.shutdown(KernelShutdownOptions::default()),
            )
            .await;
        });
    });
    assert!(handle.flush(Duration::from_secs(30)));
    handle
}

fn entries(log_path: &Path) -> Vec<serde_json::Map<String, Value>> {
    std::fs::read_to_string(log_path)
        .expect("agent.jsonl")
        .lines()
        .map(|line| match serde_json::from_str(line) {
            Ok(Value::Object(entry)) => entry,
            other => panic!("not a JSON object line: {line} ({other:?})"),
        })
        .collect()
}

/// Durations, clock times, and ids vary per run; the tree shape does not.
fn normalize(text: &str) -> String {
    let duration = Regex::new(r"  \d+(?:\.\d+)?ms  ").expect("pattern");
    let span_id = Regex::new(r"\[[0-9a-f]{16}\]").expect("pattern");
    let uuid = Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
        .expect("pattern");
    let pid = Regex::new(r"kernel\.pid=\d+").expect("pattern");
    let text = duration.replace_all(text, "  <ms>  ");
    let text = span_id.replace_all(&text, "[<span>]");
    let text = uuid.replace_all(&text, "<request>");
    pid.replace_all(&text, "kernel.pid=<pid>").into_owned()
}

#[test]
fn a_turn_with_a_kernel_cell_reconstructs_as_one_trace_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("logs").join("agent.jsonl");
    let _handle = record_a_turn(&log_path, dir.path());

    let entries = entries(&log_path);
    let turns: Vec<&serde_json::Map<String, Value>> = entries
        .iter()
        .filter(|entry| entry["msg"] == "span_end" && entry["name"] == "agent.turn")
        .collect();
    assert_eq!(turns.len(), 2, "one agent.turn per assistant turn");
    let trace_id = turns[0]["traceId"].as_str().expect("trace id").to_string();
    assert_ne!(
        turns[1]["traceId"], turns[0]["traceId"],
        "each root turn is its own trace"
    );
    // Every record carries the recorder's pid; kernel spans keep the runtime's
    // ids, hoisted error included.
    let cell = entries
        .iter()
        .find(|entry| entry["name"] == "kernel.cell")
        .expect("forwarded kernel.cell");
    assert_eq!(
        (
            cell["pid"].as_u64(),
            cell["level"].as_str(),
            cell["error"].as_str(),
            cell.contains_key("id"),
            cell.contains_key("event"),
        ),
        (
            Some(u64::from(std::process::id())),
            Some("warn"),
            Some("ZeroDivisionError: division by zero"),
            false,
            false,
        )
    );

    let outcome = pa_trace::run_trace_command(
        &[
            trace_id.clone(),
            "--log".to_string(),
            log_path.display().to_string(),
        ],
        Path::new("/unused"),
    );
    assert_eq!(
        (outcome.code, outcome.stderr.clone()),
        (0, Vec::<String>::new())
    );
    // (The scripted provider reports `stop` for its tool-call turns too.)
    let expected = [
        format!("trace {trace_id}  (6 spans, 0 log lines, {})", log_path.display()),
        "└─ agent.turn  <ms>  ok  turn.index=0 llm.provider=test llm.model=test-model turn.stop_reason=stop turn.tool_calls=1 turn.tool_errors=0  [<span>]".to_string(),
        "   ├─ llm.request  <ms>  ok  llm.provider=test llm.api=test llm.model=test-model llm.base_url= llm.stop_reason=stop llm.usage.input=0 llm.usage.output=0  [<span>]".to_string(),
        "   └─ tool.execute  <ms>  ok  tool.name=ipython tool.call_id=call-1  [<span>]".to_string(),
        "      ├─ kernel.start  <ms>  ok  kernel.python=<python> kernel.restore=false kernel.pid=<pid>  [<span>]".to_string(),
        "      └─ kernel.execute  <ms>  ok  kernel.request_type=execute kernel.request_id=<request> kernel.status=ok  [<span>]".to_string(),
        "         └─ kernel.cell  <ms>  error  kernel.request_id=<request> error=ZeroDivisionError: division by zero  error=ZeroDivisionError: division by zero  [<span>]".to_string(),
    ]
    .join("\n");
    let python = dir.path().join("fake-kernel").display().to_string();
    assert_eq!(
        normalize(&outcome.stdout.join("\n")).replace(&python, "<python>"),
        expected
    );
}
