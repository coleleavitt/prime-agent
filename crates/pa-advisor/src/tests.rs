//! The advisor over a scripted model.

use pa_agent::scripted::ScriptedProvider;
use pa_core::kernel::shared::HostRequestPayload;

use super::*;

fn test_model() -> pa_agent::types::Model {
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

fn context(session_id: &str) -> Arc<SessionFeatureContext> {
    Arc::new(SessionFeatureContext {
        agent_dir: std::path::PathBuf::from("/nonexistent-agent"),
        cwd: std::path::PathBuf::from("/nonexistent-cwd"),
        session_id: session_id.to_string(),
        python_skill_import_names: Vec::new(),
        model: test_model(),
        telemetry: None,
        rlm_depth: 0,
        session_artifact_dir: None,
    })
}

fn feature(provider: &Arc<ScriptedProvider>, selector: Option<&'static str>) -> AdvisorFeature {
    let stream_fn = provider.stream_fn();
    AdvisorFeature::with(
        Arc::new(move |_context| selector.map(str::to_string)),
        Arc::new(move |_context, _selector| SideModel::Session {
            stream_fn: Arc::clone(&stream_fn),
            model: test_model(),
        }),
    )
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::Standard(pa_agent::types::Message::User(
        pa_agent::types::UserMessage {
            content: pa_agent::types::UserContent::Text(text.to_string()),
            timestamp: 0,
        },
    ))
}

#[test]
fn without_a_model_nothing_registers_and_the_skill_hides() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    let advisor = feature(&provider, None);
    let context = context("advisor-off");
    let mut handlers = HostRequestHandlers::default();
    advisor.register_host_handlers(&context, &mut handlers);
    assert!(handlers.get(ADVISOR_REQUEST).is_none());
    assert!(!advisor.session_skill_visible(&context, ADVISOR_SKILL));
    assert!(advisor.session_skill_visible(&context, "other"));
    assert_eq!(advisor.bundled_skills(), vec![ADVISOR_SKILL]);
}

#[tokio::test]
async fn the_advisor_reads_the_conversation_and_answers() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn("Check the lockfile first.");
    let advisor = feature(&provider, Some("anthropic/strong"));
    let context = context("advisor-on");
    advisor.on_session_start(&context, &[user("earlier task")]);
    advisor.on_message_end(&context, &user("the build fails"));
    let mut handlers = HostRequestHandlers::default();
    advisor.register_host_handlers(&context, &mut handlers);
    assert!(advisor.session_skill_visible(&context, ADVISOR_SKILL));
    let handler = handlers.get(ADVISOR_REQUEST).unwrap().clone();
    let reply = handler(HostRequestPayload {
        data: json!({ "type": ADVISOR_REQUEST, "question": "why does it fail?" }),
    })
    .await
    .unwrap();
    assert_eq!(
        reply,
        json!({ "advice": "Check the lockfile first.", "model": "anthropic/strong" })
    );
    let calls = provider.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].system_prompt.as_deref(),
        Some(ADVISOR_SYSTEM_PROMPT)
    );
    let pa_agent::types::Message::User(sent) = &calls[0].messages[0] else {
        panic!("expected the consultation");
    };
    assert_eq!(
        sent.content,
        pa_agent::types::UserContent::Text(consultation_prompt(
            "User: earlier task\n\nUser: the build fails",
            "why does it fail?"
        ))
    );
    let error = handler(HostRequestPayload {
        data: json!({ "type": ADVISOR_REQUEST, "question": "  " }),
    })
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "advisor.ask question must be a non-empty string"
    );
}
