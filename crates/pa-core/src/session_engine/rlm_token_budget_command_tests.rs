//! `/rlm-token-budget` through a whole session engine: the delegation
//! budget's pool, this session's spend, and the grant each subagent drew.

use std::sync::Arc;

use pa_agent::scripted::ScriptedProvider;

use crate::session::manager::SessionManager;
use crate::session_engine::engine::{create_session, SessionEngine, SessionEngineConfig};
use crate::session_engine::session_commands::{
    execute_session_command, SessionCommandExecution, SessionCommandParams,
};
use crate::session_engine::slash_commands::{parse_session_command, SlashCommandRegistry};

fn model() -> pa_agent::types::Model {
    pa_agent::types::Model {
        id: "m".into(),
        name: "m".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 1_000,
        max_tokens: 100,
    }
}

fn wire_model() -> pa_types::ai::Model {
    serde_json::from_value(serde_json::json!({
        "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .unwrap()
}

async fn engine(root: &std::path::Path, settings: Option<&str>) -> SessionEngine {
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    if let Some(settings) = settings {
        std::fs::write(agent_dir.join("settings.json"), settings).unwrap();
    }
    let provider = Arc::new(ScriptedProvider::new(model()));
    create_session(SessionEngineConfig {
        cwd: root.to_path_buf(),
        agent_dir,
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(SessionManager::persisted(root, &root.join("sessions"))),
        rlm_depth: Some(0),
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn run(engine: &SessionEngine, text: &str) -> SessionCommandExecution {
    let command = parse_session_command(&SlashCommandRegistry::builtin(), text).unwrap();
    let mut autonomous = crate::autonomous::create_autonomous_runtime_state(None, None);
    let model = wire_model();
    let mut params = SessionCommandParams {
        model: &model,
        api_key: None,
        global_harness_dir: engine.feature_context.agent_dir.join("harness"),
        autonomous: &mut autonomous,
    };
    execute_session_command(engine, &mut params, &command).await
}

fn result_text(execution: &SessionCommandExecution) -> String {
    execution.messages.last().unwrap().content.text()
}

#[tokio::test]
async fn the_command_reports_the_pool_the_spend_and_each_grant() {
    let dir = tempfile::tempdir().unwrap();
    let unbudgeted = engine(&dir.path().join("off"), None).await;
    let off = run(&unbudgeted, "/rlm-token-budget").await;
    assert_eq!(off.error, None);
    assert_eq!(
        result_text(&off),
        "No RLM token budget applies to this session. Set rlmTokenBudget in the global settings to fund subagent delegation."
    );

    let budgeted = engine(
        &dir.path().join("on"),
        Some(r#"{"rlmTokenBudget": {"total": 1000, "perDepth": [400]}}"#),
    )
    .await;
    let budget = budgeted.rlm.token_budget.get().unwrap();
    budget.record_spend(50);
    let grant = budget.reserve_child_grant(Some(300)).unwrap();
    budget.attribute_grant(grant, "sub-a1", "auditor");
    let grant = budget.reserve_child_grant(None).unwrap();
    budget.attribute_grant(grant, "sub-b2", "lookup");
    // A spawn that failed after its grant: the grant is kept, unattributed.
    budget.reserve_child_grant(Some(100)).unwrap();

    let status = run(&budgeted, "/rlm-token-budget").await;
    assert_eq!(status.error, None);
    assert_eq!(
        result_text(&status),
        "RLM token budget (root pool, from rlmTokenBudget): 1000 tokens; any single grant to a depth-1 subagent is capped at 400\n\
         Spent by this session: 50 (the root's own spend is not capped)\n\
         Granted: 800; left to grant: 200\n\
         - auditor (sub-a1): 300\n\
         - lookup (sub-b2): 400\n\
         - not attributed (a spawn that failed after drawing its grant): 100"
    );

    let usage = run(&budgeted, "/rlm-token-budget 400k").await;
    assert_eq!(
        usage.error.as_deref(),
        Some("Usage: /rlm-token-budget (the budget is configured by the global rlmTokenBudget setting)")
    );
}
