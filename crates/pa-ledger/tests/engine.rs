//! The feature installed the way `pa-cli` installs it, under a real session
//! engine: a failing tool call is counted in the session's local harness
//! state at the next turn boundary. Its own test binary, because
//! installation is process-global.

use std::sync::Arc;
use std::time::Duration;

use pa_agent::scripted::ScriptedProvider;
use pa_core::features::SessionFeature;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_core::session_engine::tool_bridge::bridge_tool;
use pa_core::session_engine::PromptOptions;
use pa_core::{ExecutionMode, ToolDefinition};
use pa_ledger::{
    fingerprint_failure, FailureKind, FailureLedgerFeature, HarnessDocument, LedgerOptions,
};

fn failing_definition() -> ToolDefinition {
    ToolDefinition {
        name: "deploy".to_string(),
        label: "Deploy".to_string(),
        description: "Always fails".to_string(),
        prompt_snippet: String::new(),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
        execution_mode: Some(ExecutionMode::Sequential),
        prepare_arguments: None,
        execute: Arc::new(|_id, _params, _signal, _on_update| {
            // A thrown error is what the loop reports as a failed call.
            Box::pin(async move { anyhow::bail!("deploy failed: exit 3") })
        }),
    }
}

#[tokio::test]
async fn a_failing_tool_is_counted_at_the_next_turn_boundary() {
    let feature = Arc::new(FailureLedgerFeature::new(LedgerOptions {
        global_ledger: Some(true),
        ..LedgerOptions::default()
    }));
    let handle = feature.handle();
    assert!(pa_core::features::install(vec![
        Arc::clone(&feature) as Arc<dyn SessionFeature>
    ]));

    let model = pa_agent::types::Model {
        id: "m".into(),
        name: "m".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 1_000,
        max_tokens: 100,
    };
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    provider.push_tool_call_turn(None, vec![("call-1", "deploy", serde_json::json!({}))]);
    provider.push_tool_call_turn(None, vec![("call-2", "deploy", serde_json::json!({}))]);
    provider.push_text_turn("gave up");

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let agent_dir = tmp.path().join("agent");
    let engine = Box::pin(create_session(SessionEngineConfig {
        cwd,
        agent_dir: agent_dir.clone(),
        model: Some(model),
        stream_fn: Some(provider.stream_fn()),
        tools: vec![bridge_tool(failing_definition())],
        conversation_log_path: Some(tmp.path().join("sessions").join("project").join("s1.jsonl")),
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap();
    engine
        .prompt("deploy it", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    pa_core::features::flush_installed(Duration::from_secs(30));
    assert!(handle.wait_idle(Duration::from_secs(30)));

    let local = HarnessDocument::load(
        &tmp.path()
            .join("sessions")
            .join("session-artifacts")
            .join("s1")
            .join("harness"),
    )
    .failures();
    let id = fingerprint_failure(
        FailureKind::ToolError,
        Some("deploy"),
        None,
        "deploy failed: exit 3",
    )
    .id;
    let record = &local.failures[&id];
    // Branch: user (0), assistant (1, turn 1), result (2), assistant (3, turn 2),
    // result (4), assistant (5, turn 3). Each result is counted at the next
    // assistant message, on that message's turn.
    assert_eq!(
        (
            record.count,
            record.first_seen_turn,
            record.last_seen_turn,
            local.last_scanned_entry_index
        ),
        (2, 2, 3, 6)
    );
    let global = HarnessDocument::load(&agent_dir.join("harness")).failures();
    assert_eq!(global.failures[&id].count, 2);
    assert_eq!(global.last_scanned_entry_index, 0);
}
