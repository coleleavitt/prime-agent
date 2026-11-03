//! The advisor over a scripted model.

use pa_agent::scripted::ScriptedProvider;
use pa_core::kernel::shared::HostRequestPayload;
use pa_core::session_engine::model_usage::ModelUsage;
use pa_core::session_engine::rlm_usage::{
    ChildUsageAppendResult,
    RlmChildUsageFuture,
    RlmChildUsageStore,
};

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

/// A durable usage store that hands each bill to the test as it lands.
struct BillChannel(tokio::sync::mpsc::UnboundedSender<(String, ModelUsage)>);

impl RlmChildUsageStore for BillChannel {
    fn last_assistant(&self) -> RlmChildUsageFuture<'_, Option<(String, pa_types::ai::Usage)>> {
        Box::pin(async { None })
    }

    fn append_attribution(
        &self,
        _row_id: &str,
        _target_id: &str,
        _child_usage: pa_types::ai::Usage,
        _origin: Option<pa_types::session::ChildUsageOrigin>,
    ) -> RlmChildUsageFuture<'_, std::io::Result<ChildUsageAppendResult>> {
        Box::pin(async { Err(std::io::Error::other("bills only")) })
    }

    fn append_model_usage(
        &self,
        session_id: String,
        usage: ModelUsage,
    ) -> RlmChildUsageFuture<'_, std::io::Result<()>> {
        let _ = self.0.send((session_id, usage));
        Box::pin(async { Ok(()) })
    }

    fn observe_billed_rows(
        &self,
        _sink: pa_core::session::billed_row::BilledRowSink,
    ) -> RlmChildUsageFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// Register a bill channel as `session_id`'s durable store; the store lives
/// as long as the returned handle.
fn bill_channel(
    session_id: &str,
) -> (
    Arc<dyn RlmChildUsageStore>,
    tokio::sync::mpsc::UnboundedReceiver<(String, ModelUsage)>,
) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let store: Arc<dyn RlmChildUsageStore> = Arc::new(BillChannel(sender));
    pa_core::session_engine::billed_calls::register_session_usage_store(session_id, &store);
    (store, receiver)
}

/// A scripted text turn whose reply bills 40 input and 2 output tokens.
fn billed_text_turn(text: &str) -> pa_agent::scripted::ScriptedTurn {
    let mut steps = pa_agent::scripted::text_turn_steps(&test_model(), text);
    if let Some(pa_agent::scripted::ScriptStep::Event(event)) = steps.last_mut() {
        if let pa_agent::stream::AssistantMessageEvent::Done { message, .. } = &mut **event {
            message.usage.input = 40;
            message.usage.output = 2;
            message.usage.total_tokens = 42;
        }
    }
    pa_agent::scripted::ScriptedTurn::Events(steps)
}

/// The bill of one [`billed_text_turn`] reply, as its row records it.
fn expected_bill(session_id: &str, source: &str) -> serde_json::Value {
    json!([session_id, {
        "source": source, "provider": "test", "modelId": "m",
        "usage": { "input": 40, "output": 2, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 42,
                   "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } }
    }])
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

/// Each consultation sends `advisor_ask` with its outcome only.
#[tokio::test]
async fn each_consultation_reports_its_outcome() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let telemetry = {
        let recorded = Arc::clone(&recorded);
        pa_core::features::FeatureTelemetry::new(move |name, properties| {
            recorded.lock().unwrap().push((
                name.to_string(),
                properties.get("outcome").cloned(),
                properties.get("duration_ms").is_some(),
            ));
        })
    };
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn("Fine.");
    let advisor = feature(&provider, Some("anthropic/strong"));
    let mut context = (*context("advisor-telemetry")).clone();
    context.telemetry = Some(telemetry);
    let context = Arc::new(context);
    let mut handlers = HostRequestHandlers::default();
    advisor.register_host_handlers(&context, &mut handlers);
    let handler = handlers.get(ADVISOR_REQUEST).unwrap().clone();
    for question in ["ok?", ""] {
        let _ = handler(HostRequestPayload {
            data: json!({ "type": ADVISOR_REQUEST, "question": question }),
        })
        .await;
    }
    assert_eq!(
        *recorded.lock().unwrap(),
        vec![
            (ADVISOR_ASK_EVENT.to_string(), Some(json!("ok")), true),
            (ADVISOR_ASK_EVENT.to_string(), Some(json!("invalid")), true),
        ]
    );
}

/// A consultation bills the session it was asked in: the advisor's reply
/// lands as an `advisor` `model_usage` row.
#[tokio::test]
async fn a_consultation_records_its_usage_on_the_session() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_turn(billed_text_turn("Check the lockfile first."));
    let advisor = feature(&provider, Some("anthropic/strong"));
    let context = context("advisor-billed");
    let (_store, mut bills) = bill_channel("advisor-billed");
    advisor.on_session_start(&context, &[]);
    let mut handlers = HostRequestHandlers::default();
    advisor.register_host_handlers(&context, &mut handlers);
    let handler = handlers.get(ADVISOR_REQUEST).unwrap().clone();
    handler(HostRequestPayload {
        data: json!({ "type": ADVISOR_REQUEST, "question": "why does it fail?" }),
    })
    .await
    .unwrap();
    let bill = bills
        .try_recv()
        .expect("the consultation recorded its bill");
    assert_eq!(
        serde_json::to_value(bill).unwrap(),
        expected_bill("advisor-billed", "advisor")
    );
}
