// Kernel-test dispositions as pa-core's kernel verifiers: the engine's
// assembly future is large, and the scenario reads best in one body.
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! Verifier: the runtime's `rlm.workflow.run_agent` round-trips through a
//! REAL kernel to the Rust host the feature seam installs. A session turn's
//! `ipython` cell awaits one completed run and one cancelled run; the
//! runtime validates each closed reply (`validate_reply`) before the cell
//! sees it. The workflow turn runs on a faux provider the agent dir's
//! models.json registers, never on the session's own stream.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_agent::scripted::ScriptedProvider;
use pa_ai::faux::{
    faux_assistant_message, faux_text, register_faux_provider, FauxAssistantMessageOptions,
    FauxResponseStep, RegisterFauxProviderOptions,
};
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
        r#"import asyncio, json
from rlm import workflow
request = {{"protocol": "prime.workflow.run-agent/v1", "requestId": "req-1", "nodeId": "node-1", "prompt": "one turn", "model": None, "maxTurns": 1, "maxResultUtf8Bytes": 1024, "drainTimeoutMs": 5000, "tools": "none", "softTokenBudget": 1}}
out = {{}}
out["completed"] = await workflow.run_agent(request)
task = asyncio.create_task(workflow.run_agent(dict(request, requestId="req-2")))
await asyncio.sleep(0)
task.cancel()
out["cancelled"] = await task
open({receipt:?}, "w").write(json.dumps(out))
print("WORKFLOW_CELL_OK")"#,
        receipt = receipt.display().to_string(),
    )
}

#[tokio::test]
async fn the_runtime_round_trips_run_agent_through_the_rust_host() {
    let Some(kernel_python) = kernel_python() else {
        return;
    };
    assert!(pa_core::features::install(vec![Arc::new(WorkflowFeature)]));
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(agent_dir.join("sessions")).unwrap();
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let receipt = dir.path().join("workflow-receipt.json");
    let _env = EnvOverride::apply(&[
        (
            "PRIME_AGENT_KERNEL_PYTHON",
            Some(kernel_python.display().to_string()),
        ),
        ("PRIME_AGENT_CODING_AGENT_DIR", None),
        ("PRIME_API_KEY", None),
    ]);

    // The workflow turn's provider: registered in pa-ai under its own api
    // and named by the agent dir's models.json.
    let faux = register_faux_provider(RegisterFauxProviderOptions {
        api: Some("workflow-kernel-faux".to_string()),
        provider: Some("workflow-kernel".to_string()),
        ..RegisterFauxProviderOptions::default()
    });
    faux.set_responses(vec![
        FauxResponseStep::Message(faux_assistant_message(
            vec![faux_text("hé"), faux_text("llo")],
            FauxAssistantMessageOptions::default(),
        )),
        // Held in flight until the cancellation reaches the transport.
        FauxResponseStep::Delayed {
            message: faux_assistant_message(
                vec![faux_text("late")],
                FauxAssistantMessageOptions::default(),
            ),
            delay_ms: 600_000,
        },
    ]);
    std::fs::write(
        agent_dir.join("models.json"),
        json!({ "providers": { "workflow-kernel": {
            "baseUrl": "http://127.0.0.1:9", "apiKey": "workflow-key",
            "api": "workflow-kernel-faux",
            "models": [{ "id": "faux-1", "name": "Faux", "contextWindow": 128_000 }]
        } } })
        .to_string(),
    )
    .unwrap();

    // The session turn: one ipython call running the cell, then a reply.
    let model: pa_agent::types::Model = serde_json::from_value(json!({
        "id": "faux-1", "name": "Faux", "api": "workflow-kernel-faux",
        "provider": "workflow-kernel", "baseUrl": "http://127.0.0.1:9", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 128_000, "maxTokens": 4_000
    }))
    .unwrap();
    let session_provider = Arc::new(ScriptedProvider::new(model.clone()));
    session_provider.push_tool_call_turn(
        Some("running the workflow cell"),
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
        .prompt("run the workflow cell", PromptOptions::default())
        .await
        .unwrap();
    assert_eq!(outcome, PromptOutcome::Prompt);

    let raw = std::fs::read_to_string(&receipt)
        .unwrap_or_else(|_| panic!("the cell wrote no receipt at {}", receipt.display()));
    let mut payload: Value = serde_json::from_str(&raw).unwrap();
    for key in ["completed", "cancelled"] {
        assert!(payload[key]["durationMs"].is_u64(), "{payload}");
        payload[key]["durationMs"] = json!(0);
    }
    let usage = |input: u64, output: u64, total: u64| {
        json!({
            "inputTokens": input, "outputTokens": output, "cacheReadTokens": 0,
            "cacheWriteTokens": 0, "totalTokens": total, "costInput": 0.0,
            "costOutput": 0.0, "costCacheRead": 0.0, "costCacheWrite": 0.0,
            "costTotal": 0.0, "completeness": "complete_host_observation",
            "finality": "final"
        })
    };
    assert_eq!(
        payload["completed"],
        json!({
            "protocol": "prime.workflow.run-agent-result/v1",
            "requestId": "req-1",
            "nodeId": "node-1",
            "resolvedModel": "workflow-kernel/faux-1",
            "turnsStarted": 1,
            "durationMs": 0,
            "budgetExhausted": true,
            // The faux provider's own usage estimate (prompt and answer
            // characters / 4), against the soft budget of 1.
            "budgetOvershootTokens": 5,
            "usage": usage(4, 2, 6),
            "outcome": "completed",
            "stopReason": "completed",
            "result": {
                "text": "héllo",
                "utf8Bytes": 6,
                "sha256": "3c48591d8d098a4538f5e013dfcf406e948eac4d3277b10bf614e295d6068179"
            },
            "error": null
        })
    );
    // The cancel may land before or after dispatch; either way the reply
    // is the closed cancelled variant.
    let cancelled = &payload["cancelled"];
    assert_eq!(
        (
            &cancelled["requestId"],
            &cancelled["outcome"],
            &cancelled["stopReason"],
            &cancelled["result"],
            &cancelled["error"],
        ),
        (
            &json!("req-2"),
            &json!("cancelled"),
            &json!("caller_aborted"),
            &Value::Null,
            &Value::Null,
        ),
        "{payload}"
    );
    // The session stream served only the session's own two turns.
    assert_eq!(session_provider.calls().len(), 2);
    faux.unregister();
}
