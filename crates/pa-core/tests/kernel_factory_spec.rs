// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines/casts).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Drives a real kernel process; unix-only like the sibling kernel targets.
#![cfg(unix)]

//! The kernel's factory validator client over a real kernel: the
//! synchronous `rlm.factory` validator functions (and the `rlm.harness`
//! factory writes behind them) reach the host's single validator through
//! the blocking `factory.spec` host request, from inside a running cell.
//! The kernel Python is ambient product state; skipped when absent.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::HostRequestHandlers;
use pa_core::{IpythonToolOptions, ToolContentBlock, create_ipython_tool_definition};
use serde_json::{Value, json};

fn kernel_python() -> Option<PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

async fn run_cell(tool: &pa_core::ToolDefinition, code: &str) -> (String, bool) {
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        (tool.execute)("call", json!({ "code": code }), None, None),
    )
    .await
    .unwrap_or_else(|_| panic!("cell {code:?} never settled"))
    .expect("cell runs");
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            ToolContentBlock::Text { text } => Some(text.as_str()),
            ToolContentBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("");
    (text, result.is_error)
}

#[tokio::test]
async fn the_validator_client_answers_from_the_host_inside_a_cell() {
    let Some(python) = kernel_python() else {
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let mut host_handlers = HostRequestHandlers::new();
    pa_core::factory::host::register_session_free_factory_handlers(&mut host_handlers);
    let provisioner = IpythonKernelProvisioner::new(
        dir.path(),
        IpythonKernelProvisionerOptions {
            python: Some(python),
            host_handlers,
            ..Default::default()
        },
    );
    let tool = create_ipython_tool_definition(
        "/tmp",
        IpythonToolOptions {
            provisioner: Arc::new(provisioner),
            ui: None,
            on_late_sent_agent_message: None,
            cell_timeout_ms: None,
        },
    );
    // Every validator call is a blocking host request from the loop thread;
    // the cell also times a batch of round trips (the write path's added
    // latency, reported below).
    let cell = r#"import json, time
from rlm.factory import validate_factory_spec, canonicalize_factory_spec, compile_factory_dag
spec = {"run": {"max_parallel": 2}, "nodes": [
    {"id": "a", "subagent": "worker", "outputs": [{"name": "o", "type": "text"}]},
    {"id": "b", "subagent": {"prompt": "Use {i}"}, "depends_on": ["a"],
     "inputs": [{"name": "i", "type": "text", "from": "a.o"}]},
]}
bad = {"nodes": [{"id": "a", "subagent": "worker", "budget_ms": float("nan")}]}
start = time.perf_counter()
for _ in range(100):
    validate_factory_spec(spec)
per_call_ms = (time.perf_counter() - start) * 10
canonical = canonicalize_factory_spec(spec)
machine, errors = compile_factory_dag(spec)
try:
    canonicalize_factory_spec({"nodes": []})
    refusal = None
except ValueError as error:
    refusal = str(error)
print(json.dumps({
    "valid": validate_factory_spec(spec),
    "bad": validate_factory_spec(bad),
    "states": [state["id"] for state in canonical["states"]],
    "join": machine["transitions"],
    "refusal": refusal,
    "per_call_ms": per_call_ms,
}))"#;
    let (text, is_error) = run_cell(&tool, cell).await;
    assert!(!is_error, "{text}");
    let mut reply: Value =
        serde_json::from_str(text.trim()).unwrap_or_else(|error| panic!("{error}: {text}"));
    let per_call_ms = reply
        .as_object_mut()
        .and_then(|object| object.remove("per_call_ms"))
        .and_then(|value| value.as_f64())
        .expect("timing");
    eprintln!(
        "factory.spec blocking host round trip: {per_call_ms:.3} ms per validate_factory_spec"
    );
    assert_eq!(
        reply,
        json!({
            "valid": [],
            "bad": ["node a budget_ms must be a positive integer"],
            "states": ["a", "b"],
            "join": [{ "from": "a", "to": "b" }],
            "refusal": "factory dag must declare between 1 and 1024 nodes, got 0",
        })
    );
}
