//! The `fallbackModels` chain end to end (upstream #1465): a session model
//! whose only provider keeps failing hands the turn to the next configured
//! model on the same conversation, and the settle restores the session model.

use crate::agent_engine::tests::FAUX_TEST_LOCK;
use crate::agent_engine::{AgentEngineConfig, AgentSessionEngine};
use crate::engine::{EngineEvent, EngineModelSelection, PromptRequest, SessionEngine as _};

fn faux_model(id: &str) -> pa_ai::faux::FauxModelDefinition {
    pa_ai::faux::FauxModelDefinition {
        id: id.to_string(),
        name: Some(id.to_string()),
        reasoning: Some(false),
        input: Some(vec![pa_types::ai::ModelInput::Text]),
        cost: None,
        context_window: Some(128_000),
        max_tokens: Some(4096),
    }
}

fn failure(text: &str) -> pa_ai::faux::FauxResponseStep {
    pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
        text,
        pa_ai::faux::FauxAssistantMessageOptions {
            stop_reason: Some(pa_types::ai::StopReason::Error),
            error_message: Some("primary is down".to_string()),
            ..Default::default()
        },
    ))
}

/// `primary` serves only the session model, `fallback` only the chain's
/// model: no same-model provider failover exists, so only the chain can
/// keep the turn alive.
fn fallback_engine(dir: &std::path::Path, settings: &serde_json::Value) -> AgentSessionEngine {
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let provider = |api: &str, id: &str| {
        serde_json::json!({
            "api": api,
            "baseUrl": "http://127.0.0.1:9",
            "apiKey": format!("sk-{api}"),
            "models": [{
                "id": id, "name": id, "api": api,
                "contextWindow": 128_000, "maxTokens": 4096
            }]
        })
    };
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::json!({
            "providers": {
                "primary": provider("mock-fallback-primary", "mock-1"),
                "fallback": provider("mock-fallback-backup", "mock-2"),
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
    let engine = AgentSessionEngine::new(AgentEngineConfig {
        cwd: dir.to_path_buf(),
        agent_dir,
        provider: None,
        model: None,
        api_key: None,
        thinking: None,
        session_dir: None,
        session_file: None,
        faux_script: None,
        supervisor_link: None,
        telemetry_disabled: None,
        cron_store: None,
        queued_steering_probe: None,
    })
    .unwrap();
    engine.configure_model(EngineModelSelection {
        provider: Some("primary".to_string()),
        model: Some("mock-1".to_string()),
        api_key: None,
        thinking: None,
    });
    engine
}

fn run(engine: &AgentSessionEngine) -> Vec<EngineEvent> {
    let mut events = Vec::new();
    engine.run_prompt(
        0,
        PromptRequest {
            batch: Vec::new(),
            images: Vec::new(),
            message: "hello".to_string(),
            source: "user".to_string(),
            agent_message_id: None,
            custom_message: None,
        },
        &|| false,
        &mut |event| {
            events.push(event);
            true
        },
    );
    events
}

/// `(provider, model, stopReason)` of every settled assistant turn.
fn turn_ends(events: &[EngineEvent]) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::TurnEnd { message, .. } => Some((
                message["provider"].as_str().unwrap_or_default().to_string(),
                message["model"].as_str().unwrap_or_default().to_string(),
                message["stopReason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn a_spent_session_model_continues_on_the_configured_fallback_model() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let primary = pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
        api: Some("mock-fallback-primary".to_string()),
        provider: Some("primary".to_string()),
        models: Some(vec![faux_model("mock-1")]),
        ..Default::default()
    });
    let backup = pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
        api: Some("mock-fallback-backup".to_string()),
        provider: Some("fallback".to_string()),
        models: Some(vec![faux_model("mock-2")]),
        ..Default::default()
    });
    primary.set_responses(vec![failure("down")]);
    backup.set_responses(vec![pa_ai::faux::FauxResponseStep::Message(
        pa_ai::faux::faux_assistant_text_message(
            "served by the fallback model",
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        ),
    )]);
    let dir = tempfile::TempDir::new().unwrap();
    // `maxRetries: 0` spends the primary's budget on its first failure.
    let engine = fallback_engine(
        dir.path(),
        &serde_json::json!({
            "fallbackModels": ["nope/missing", "fallback/mock-2"],
            "retry": { "failover": { "maxRetries": 0, "baseDelayMs": 1 } }
        }),
    );
    let events = run(&engine);
    assert_eq!(primary.call_count(), 1);
    assert_eq!(backup.call_count(), 1);
    assert_eq!(
        turn_ends(&events),
        vec![
            (
                "primary".to_string(),
                "mock-1".to_string(),
                "error".to_string()
            ),
            (
                "fallback".to_string(),
                "mock-2".to_string(),
                "stop".to_string()
            ),
        ]
    );
    let backup_start = events.iter().find_map(|event| match event {
        EngineEvent::AutoRetryStart { reason, .. } => Some(reason.clone()),
        _ => None,
    });
    assert_eq!(
        backup_start,
        Some(
            pa_core::session_engine::auto_retry::RetryStartReason::Backup {
                backup_model: "fallback/mock-2".to_string()
            }
        )
    );
    // The settle restores the session model for the next turn.
    assert_eq!(engine.session_model().unwrap().id, "mock-1");
    primary.unregister();
    backup.unregister();
}

/// The default (no `fallbackModels`) keeps the native give-up on the
/// session model.
#[test]
fn without_a_fallback_chain_the_turn_fails_on_the_session_model() {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let primary = pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
        api: Some("mock-fallback-primary".to_string()),
        provider: Some("primary".to_string()),
        models: Some(vec![faux_model("mock-1")]),
        ..Default::default()
    });
    let backup = pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
        api: Some("mock-fallback-backup".to_string()),
        provider: Some("fallback".to_string()),
        models: Some(vec![faux_model("mock-2")]),
        ..Default::default()
    });
    primary.set_responses(vec![failure("down 1"), failure("down 2")]);
    let dir = tempfile::TempDir::new().unwrap();
    let engine = fallback_engine(
        dir.path(),
        &serde_json::json!({
            "retry": {
                "maxRetries": 1,
                "baseDelayMs": 1,
                "failover": { "maxRetries": 0, "baseDelayMs": 1 }
            }
        }),
    );
    let events = run(&engine);
    assert_eq!(backup.call_count(), 0);
    assert_eq!(
        turn_ends(&events),
        vec![
            (
                "primary".to_string(),
                "mock-1".to_string(),
                "error".to_string()
            ),
            (
                "primary".to_string(),
                "mock-1".to_string(),
                "error".to_string()
            ),
        ]
    );
    primary.unregister();
    backup.unregister();
}
