//! Coordinator and brief mode through a whole session engine: the prompt
//! sections, the coordinator's gate, `/brief`, `rlm.send_user_message`, and
//! the stop enforcement.

use std::sync::Arc;

use pa_agent::scripted::ScriptedProvider;
use serde_json::{Value, json};

use crate::kernel::shared::HostRequestPayload;
use crate::session::manager::SessionManager;
use crate::session_engine::PromptOptions;
use crate::session_engine::brief_mode::{
    BRIEF_ENFORCE_SENTINEL,
    SEND_USER_MESSAGE_REQUEST,
    USER_MESSAGE_CUSTOM_TYPE,
};
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

async fn engine(
    dir: &std::path::Path,
    provider: &Arc<ScriptedProvider>,
    settings: &str,
) -> SessionEngine {
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("settings.json"), settings).unwrap();
    create_session(SessionEngineConfig {
        cwd: dir.to_path_buf(),
        agent_dir,
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
    let model: pa_types::ai::Model = serde_json::from_value(json!({
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
    execute_session_command(engine, &mut params, &command).await
}

async fn prompt(engine: &SessionEngine, text: &str) {
    engine.prompt(text, PromptOptions::default()).await.unwrap();
    engine.session.agent().wait_for_idle().await;
}

fn request_text(call: &pa_agent::stream::LlmContext) -> String {
    serde_json::to_string(&call.messages).unwrap()
}

async fn review(engine: &SessionEngine, request: &str, data: Value) -> Option<String> {
    let gate = engine.provisioner.host_handlers().gate().cloned();
    match gate {
        Some(gate) => gate.review(request, &data).await,
        None => None,
    }
}

#[tokio::test]
async fn a_coordinator_session_gets_the_section_and_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let coordinator = engine(
        dir.path(),
        &provider,
        r#"{"experimental": {"coordinatorMode": true, "coordinatorExtraTools": ["WebFetch"]}}"#,
    )
    .await;
    assert!(coordinator.system_prompt.contains("# Coordinator mode"));
    assert!(coordinator.modes.coordinator.is_some());
    assert!(
        review(&coordinator, "bash.run", Value::Null)
            .await
            .is_some()
    );
    assert!(
        review(&coordinator, "file.edit", Value::Null)
            .await
            .is_some()
    );
    assert_eq!(review(&coordinator, "web.fetch", Value::Null).await, None);
    assert_eq!(review(&coordinator, "rlm.collect", Value::Null).await, None);

    let other = tempfile::tempdir().unwrap();
    let plain = engine(other.path(), &provider, "{}").await;
    assert!(!plain.system_prompt.contains("# Coordinator mode"));
    assert_eq!(review(&plain, "bash.run", Value::Null).await, None);
}

#[tokio::test]
async fn brief_mode_enforces_a_user_message_once_per_run() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let brief = engine(
        dir.path(),
        &provider,
        r#"{"experimental": {"briefMode": true}}"#,
    )
    .await;
    assert!(brief.system_prompt.contains("## Talking to the user"));
    provider.push_text_turn("plain answer");
    provider.push_text_turn("still plain");
    prompt(&brief, "hi").await;
    let calls = provider.calls();
    assert_eq!(
        calls.len(),
        2,
        "one enforcement continuation, then the stop"
    );
    assert!(!request_text(&calls[0]).contains(BRIEF_ENFORCE_SENTINEL));
    assert!(request_text(&calls[1]).contains(BRIEF_ENFORCE_SENTINEL));

    // Off: a plain reply ends the run.
    let other = tempfile::tempdir().unwrap();
    let plain_provider = Arc::new(ScriptedProvider::new(model()));
    let plain = engine(other.path(), &plain_provider, "{}").await;
    assert!(!plain.system_prompt.contains("## Talking to the user"));
    plain_provider.push_text_turn("plain answer");
    prompt(&plain, "hi").await;
    assert_eq!(plain_provider.calls().len(), 1);
}

#[tokio::test]
async fn the_brief_command_switches_the_mode_and_tells_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    let engine = engine(dir.path(), &provider, "{}").await;
    let send = engine
        .provisioner
        .host_handlers()
        .get(SEND_USER_MESSAGE_REQUEST)
        .expect("registered")
        .clone();
    assert!(
        send(HostRequestPayload {
            data: json!({ "message": "hello" })
        })
        .await
        .is_err(),
        "refused outside brief mode"
    );

    let on = run_command(&engine, "/brief").await;
    assert_eq!(on.error, None);
    let types: Vec<&str> = on
        .messages
        .iter()
        .map(|row| row.custom_type.as_str())
        .collect();
    assert_eq!(
        types,
        [
            pa_types::slash_commands::SESSION_SLASH_COMMAND_CUSTOM_TYPE,
            crate::session_engine::brief_mode::BRIEF_MODE_CHANGE_CUSTOM_TYPE,
            crate::session_engine::brief_mode::BRIEF_MODE_REMINDER_CUSTOM_TYPE,
        ]
    );
    assert!(engine.modes.brief.is_on());
    assert_eq!(
        engine.session.session_handle().lock().await.brief_mode(),
        Some(true)
    );

    // The message records its displayed row, which the model never reads
    // back; the reminder reaches the next request.
    send(HostRequestPayload {
        data: json!({ "message": "Shipped fix 4242" }),
    })
    .await
    .unwrap();
    // The message went out before this prompt: the run is reminded once.
    provider.push_text_turn("ok");
    provider.push_text_turn("ok again");
    prompt(&engine, "next").await;
    let request = request_text(provider.calls().last().unwrap());
    assert!(request.contains("Brief mode is now enabled."));
    assert!(!request.contains("Brief-only mode enabled"));
    assert!(!request.contains("Shipped fix 4242"));
    let recorded = engine
        .session
        .session_handle()
        .lock()
        .await
        .get_all_entries()
        .iter()
        .any(|entry| {
            matches!(entry, pa_types::session::FileEntry::CustomMessage { payload, .. }
                if payload.custom_type == USER_MESSAGE_CUSTOM_TYPE)
        });
    assert!(recorded);

    let off = run_command(&engine, "/brief off").await;
    assert_eq!(off.error, None);
    assert!(!engine.modes.brief.is_on());
    let again = run_command(&engine, "/brief off").await;
    assert_eq!(again.messages.len(), 2, "the echo and 'already off'");
}
