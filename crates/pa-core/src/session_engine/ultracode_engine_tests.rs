//! Ultracode through a whole session engine: the keyword's reminder rides the
//! turn's request, `/ultracode` records the durable change and the model hears
//! the entry and exit reminders, never the change row itself.

use std::sync::Arc;

use pa_agent::scripted::ScriptedProvider;

use super::*;
use crate::session::manager::SessionManager;
use crate::session_engine::PromptOptions;
use crate::session_engine::engine::{SessionEngine, SessionEngineConfig, create_session};
use crate::session_engine::session_commands::{
    SessionCommandExecution,
    SessionCommandParams,
    execute_session_command,
};
use crate::session_engine::slash_commands::{SlashCommandRegistry, parse_session_command};

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
        max_tokens_explicit: false,
    }
}

async fn engine(dir: &std::path::Path, provider: &Arc<ScriptedProvider>) -> SessionEngine {
    create_session(SessionEngineConfig {
        cwd: dir.to_path_buf(),
        agent_dir: dir.join("agent"),
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        session_manager: Some(SessionManager::in_memory(dir)),
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn run_command(engine: &SessionEngine, text: &str) -> SessionCommandExecution {
    let command = parse_session_command(&SlashCommandRegistry::builtin(), text).unwrap();
    let mut autonomous = crate::autonomous::create_autonomous_runtime_state(None, None);
    let model: pa_types::ai::Model = serde_json::from_value(serde_json::json!({
        "id": "m", "name": "m", "api": "openai-completions", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .unwrap();
    let global_harness = tempfile::tempdir().expect("temp dir");
    let mut params = SessionCommandParams {
        model: &model,
        api_key: None,
        global_harness_dir: global_harness.path().to_path_buf(),
        autonomous: &mut autonomous,
    };
    execute_session_command(engine, &mut params, &command, &[]).await
}

async fn turn(engine: &SessionEngine, provider: &ScriptedProvider, text: &str) -> String {
    provider.push_text_turn("ok");
    engine.prompt(text, PromptOptions::default()).await.unwrap();
    engine.session.agent().wait_for_idle().await;
    let calls = provider.calls();
    let last = calls.last().expect("the turn reached the provider");
    serde_json::to_string(&last.messages).unwrap()
}

/// The newest user turn's part of a request: everything after the previous
/// assistant reply.
fn newest_turn(request: &str) -> &str {
    request.rsplit("\"ok\"").next().unwrap_or(request)
}

#[tokio::test]
async fn the_keyword_and_the_standing_mode_reach_the_model_as_reminders() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = engine(dir.path(), &provider).await;

    let keyword = turn(&engine, &provider, "ultracode: audit the parser").await;
    assert!(
        keyword.contains("opting this turn into multi-agent orchestration"),
        "{keyword}"
    );
    assert!(keyword.contains("keep workflows under 10 agents"));
    let plain = turn(&engine, &provider, "thanks").await;
    assert!(!newest_turn(&plain).contains("opting this turn"), "{plain}");
    // Quoted, the word does not opt in.
    let quoted = turn(&engine, &provider, "what does `ultracode` mean?").await;
    assert!(!newest_turn(&quoted).contains("opting this turn"));

    let on = run_command(&engine, "/ultracode on").await;
    assert_eq!(on.error, None);
    assert_eq!(
        on.messages
            .iter()
            .map(|row| row.custom_type.as_str())
            .collect::<Vec<_>>(),
        vec!["session_slash_command", ULTRACODE_CHANGE_CUSTOM_TYPE]
    );
    let entered = turn(&engine, &provider, "refactor the lexer").await;
    assert!(
        newest_turn(&entered).contains("Ultracode is on: optimize"),
        "{entered}"
    );
    assert!(
        !entered.contains("Ultracode on: every substantive task"),
        "the change row stays out"
    );
    let quiet = turn(&engine, &provider, "and the tests").await;
    assert!(!newest_turn(&quiet).contains("Ultracode is"), "{quiet}");

    let status = run_command(&engine, "/ultracode status").await;
    assert!(matches!(
        &status.messages[1].content,
        pa_types::ai::UserContent::Text(text) if text == "Ultracode is on."
    ));
    run_command(&engine, "/ultracode off").await;
    let exited = turn(&engine, &provider, "done").await;
    assert!(
        newest_turn(&exited).contains("Ultracode is off"),
        "{exited}"
    );

    let entries = engine.session.entries().await;
    assert_eq!(ultracode_in_entries(entries.iter()), Some(false));
    let bad = run_command(&engine, "/ultracode maybe").await;
    assert_eq!(
        bad.error.as_deref(),
        Some("Usage: /ultracode [on|off|status]")
    );
}
