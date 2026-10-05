// Kernel-test dispositions as pa-core's kernel verifiers: the engine's
// assembly future is large, and the scenario reads best in one body.
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! Verifier: the runtime's `rlm.workflow_v2.request` round-trips through a
//! REAL kernel (the bootstrapped, wheel-installed runtime) to the Rust host
//! the feature seam installs. A session turn's `ipython` cell validates a
//! definition (the host's canonical digest comes back), asks for a run's
//! status (the host's closed `CAPABILITY_UNAVAILABLE` error comes back), and
//! tries a mutating `create` (the runtime refuses it before any host call).
//! The runtime re-validates every closed reply before the cell sees it. No
//! provider is involved beyond the scripted session stream.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_agent::scripted::ScriptedProvider;
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_core::session_engine::{PromptOptions, PromptOutcome};
use pa_workflow::WorkflowFeature;
use serde_json::{json, Value};

/// The kernel Python with prime-agent-runtime installed. Skipped (with a
/// note) on machines without a bootstrapped kernel venv.
fn kernel_python() -> Option<PathBuf> {
    let candidate =
        PathBuf::from(std::env::var("HOME").ok()?).join(".prime/agent/kernel-venv/bin/python");
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live kernel test",
        candidate.display()
    );
    None
}

/// Scoped process-env overrides, restored on drop. This binary carries
/// exactly one test, so nothing races.
struct EnvOverride(Vec<(&'static str, Option<String>)>);

impl EnvOverride {
    fn apply(pairs: &[(&'static str, Option<String>)]) -> Self {
        let saved = pairs
            .iter()
            .map(|(key, _)| (*key, std::env::var(key).ok()))
            .collect();
        for (key, value) in pairs {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        EnvOverride(saved)
    }
}

impl Drop for EnvOverride {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn harness_cell(receipt: &Path) -> String {
    format!(
        r#"import json
from rlm import workflow_v2
definition = {{"protocol": "prime.workflow.definition/v2", "nodes": [{{"nodeId": "n", "kind": "agent", "prompt": "hi", "dependsOn": [], "model": "m", "maxTurns": 1, "tools": "none", "maxTokens": 10}}], "outputs": ["n"], "budget": {{"maxConcurrentAttempts": 1, "maxTotalTokens": 10, "semantics": "soft_admission"}}}}
base = {{"protocol": workflow_v2.REQUEST_PROTOCOL}}
out = {{}}
out["validate"] = await workflow_v2.request(request=dict(base, requestId="req-1", action="validate", definition=definition))
out["status"] = await workflow_v2.request(request=dict(base, requestId="req-2", action="status", runId="run-1"))
try:
    await workflow_v2.request(request=dict(base, requestId="req-3", action="create", definition=definition))
    out["create"] = "sent"
except workflow_v2.CapabilityUnavailable as exc:
    out["create"] = exc.code
open({receipt:?}, "w").write(json.dumps(out))
print("WORKFLOW_V2_CELL_OK")"#,
        receipt = receipt.display().to_string(),
    )
}

#[tokio::test]
async fn the_runtime_round_trips_workflow_v2_through_the_rust_host() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    assert!(pa_core::features::install(vec![Arc::new(WorkflowFeature)]));
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).unwrap();
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let receipt = dir.path().join("workflow-v2-receipt.json");
    let _env = EnvOverride::apply(&[
        (
            "PRIME_AGENT_KERNEL_PYTHON",
            Some(kernel_python.display().to_string()),
        ),
        ("PRIME_AGENT_CODING_AGENT_DIR", None),
        ("PRIME_API_KEY", None),
    ]);

    // The session turn: one ipython call running the cell, then a reply.
    let model: pa_agent::types::Model = serde_json::from_value(json!({
        "id": "scripted-1", "name": "Scripted", "api": "workflow-v2-scripted",
        "provider": "workflow-v2", "baseUrl": "http://127.0.0.1:9", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 128_000, "maxTokens": 4_000
    }))
    .unwrap();
    let session_provider = Arc::new(ScriptedProvider::new(model.clone()));
    session_provider.push_tool_call_turn(
        Some("running the workflow v2 cell"),
        vec![(
            "call-1",
            "ipython",
            json!({ "code": harness_cell(&receipt) }),
        )],
    );
    session_provider.push_text_turn("done");

    let mut session = SessionManager::in_memory(&cwd);
    session.materialize_session_file(Some(agent_dir.join("sessions")));
    let engine = create_session(SessionEngineConfig {
        plan_mode: None,
        on_late_sent_agent_message: None,
        semantic_edges: None,
        cron_store: None,
        queued_steering_probe: None,
        image_model_router: None,
        steering_mode: None,
        follow_up_mode: None,
        cwd,
        agent_dir,
        model: Some(model),
        thinking_level: None,
        stream_fn: Some(session_provider.stream_fn()),
        tools: Vec::new(),
        custom_system_prompt: None,
        prompt_guidelines: Vec::new(),
        generic_mcp_servers: Vec::new(),
        allow_recursion: None,
        session_manager: Some(session),
        extra_host_handlers: None,
        conversation_log_path: None,
        additional_skill_paths: Vec::new(),
        additional_prompt_paths: Vec::new(),
        extra_builtin_skill_overrides: Vec::new(),
        rlm_subagent_host: None,
        rlm_depth: None,
        telemetry: None,
        model_info: None,
        mcp_manager: None,
        prewarm_ipython_kernel: None,
        on_background_work_settled: None,
        queued_goal_context_purge: None,
    })
    .await
    .unwrap();

    let outcome = engine
        .prompt("run the workflow v2 cell", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(outcome, PromptOutcome::Prompt);

    let raw = std::fs::read_to_string(&receipt)
        .unwrap_or_else(|_| panic!("the cell wrote no receipt at {}", receipt.display()));
    let payload: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        payload,
        json!({
            "validate": {
                "protocol": "prime.workflow.result/v2",
                "requestId": "req-1",
                "action": "validate",
                "valid": true,
                // sha256 over the definition's RFC 8785 bytes, computed
                // independently (Python's sorted compact dump).
                "definitionDigest": "sha256:ac3260986589ff84071f3f1cadd684ac7fa684344bba88f470f549730a551794",
                "errors": [],
                "warnings": []
            },
            "status": {
                "protocol": "prime.workflow.error/v2",
                "requestId": "req-2",
                "code": "CAPABILITY_UNAVAILABLE",
                "message": "Workflow V2 status is unavailable: this host has no durable workflow controller",
                "retryable": false,
                "currentRevision": null
            },
            "create": "CAPABILITY_UNAVAILABLE"
        })
    );
    // The session stream served only the session's own two turns.
    assert_eq!(session_provider.calls().len(), 2);
}
