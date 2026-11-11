//! The observer feature end to end over a scripted observer model.

use std::time::Duration;

use pa_agent::scripted::ScriptedProvider;
use pa_core::session_engine::model_usage::ModelUsage;
use pa_core::session_engine::rlm_usage::{
    ChildUsageAppendResult,
    RlmChildUsageFuture,
    RlmChildUsageStore,
};
use serde_json::json;

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

fn context(session_id: &str, depth: u32) -> Arc<SessionFeatureContext> {
    Arc::new(SessionFeatureContext {
        agent_dir: std::path::PathBuf::from("/nonexistent-agent"),
        cwd: std::path::PathBuf::from("/nonexistent-cwd"),
        session_id: session_id.to_string(),
        python_skill_import_names: Vec::new(),
        model: test_model(),
        telemetry: None,
        rlm_depth: depth,
        session_artifact_dir: None,
    })
}

fn feature(provider: &Arc<ScriptedProvider>, enabled: bool) -> ObserverFeature {
    let stream_fn = provider.stream_fn();
    ObserverFeature::with(
        Arc::new(move |_context| {
            enabled.then_some(ObserverSettings {
                model: None,
                instructions: None,
            })
        }),
        Arc::new(move |_context, _selector| SideModel::Session {
            stream_fn: Arc::clone(&stream_fn),
            model: test_model(),
        }),
    )
    .with_nonce("n0")
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
    AgentMessage::Standard(user_row(text))
}

fn assistant(text: &str) -> AgentMessage {
    AgentMessage::Standard(assistant_row(text))
}

fn texts(call: &pa_agent::stream::LlmContext) -> Vec<String> {
    call.messages
        .iter()
        .map(|message| match message {
            Message::User(user) => match &user.content {
                UserContent::Text(text) => format!("user: {text}"),
                UserContent::Parts(_) => "user: [parts]".to_string(),
            },
            Message::Assistant(assistant) => format!(
                "assistant: {}",
                assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            ),
            Message::ToolResult(_) => "tool".to_string(),
        })
        .collect()
}

async fn wait_for_calls(provider: &ScriptedProvider, count: usize) {
    for _ in 0..300 {
        if provider.calls().len() >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the observer was called {} times, expected {count}",
        provider.calls().len()
    );
}

#[tokio::test]
async fn a_disabled_or_too_deep_session_is_never_observed() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    let off = feature(&provider, false);
    let context_off = context("observer-off", 0);
    off.on_session_start(&context_off, &[]);
    off.on_message_end(&context_off, &user("hi"));
    off.on_agent_end(&context_off);
    let on = feature(&provider, true);
    let deep = context("observer-deep", MAX_OBSERVED_DEPTH + 1);
    on.on_session_start(&deep, &[]);
    on.on_message_end(&deep, &user("hi"));
    on.on_agent_end(&deep);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(provider.calls().is_empty());
}

#[tokio::test]
async fn silence_delivers_nothing_and_a_report_reaches_the_observed_session() {
    let session_id = "observer-reports";
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn("NO_REPORT");
    provider.push_text_turn("It edited the generated file instead of the template.");
    let notices = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink: pa_core::features::FeatureNoticeSink = {
        let notices = Arc::clone(&notices);
        Arc::new(move |row: pa_types::session::CustomMessage| {
            if let pa_types::ai::UserContent::Text(text) = row.content {
                notices.lock().unwrap().push(text);
            }
        })
    };
    pa_core::features::register_feature_notice_sink(session_id, &sink);
    let observer = feature(&provider, true);
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut context = (*context(session_id, 0)).clone();
    context.telemetry = Some({
        let recorded = Arc::clone(&recorded);
        pa_core::features::FeatureTelemetry::new(move |name, properties| {
            recorded
                .lock()
                .unwrap()
                .push((name.to_string(), properties));
        })
    });
    let context = Arc::new(context);
    observer.on_session_start(&context, &[]);

    observer.on_message_end(&context, &user("render the docs"));
    observer.on_message_end(&context, &assistant("Rendering."));
    observer.on_agent_end(&context);
    wait_for_calls(&provider, 1).await;
    let first_digest = render_digest(
        "main",
        &[
            ActivityItem::UserMessage("render the docs".into()),
            ActivityItem::AssistantText("Rendering.".into()),
            ActivityItem::TurnEnded {
                reason: "end".into(),
            },
        ],
        "n0",
        None,
    );
    let calls = provider.calls();
    assert_eq!(
        calls[0].system_prompt.as_deref(),
        Some(system_prompt("main").as_str())
    );
    assert!(calls[0].tools.is_empty());
    assert_eq!(texts(&calls[0]), vec![format!("user: {first_digest}")]);

    observer.on_message_end(&context, &assistant("Edited docs/out.html."));
    observer.on_agent_end(&context);
    wait_for_calls(&provider, 2).await;
    for _ in 0..300 {
        if !notices.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The second delivery carries the first exchange ahead of the new
    // digest.
    let second = texts(&provider.calls()[1]);
    assert_eq!(second.len(), 3);
    assert_eq!(second[0], format!("user: {first_digest}"));
    assert_eq!(second[1], "assistant: NO_REPORT");
    assert_eq!(
        *notices.lock().unwrap(),
        vec![report_notice(
            "observer",
            "It edited the generated file instead of the template."
        )]
    );
    // A turn without activity sends nothing.
    observer.on_agent_end(&context);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(provider.calls().len(), 2);
    drop(sink);
    // The session's end reports its counts (`observer_session`).
    observer.on_session_end(&context, "quit");
    let mut expected = pa_telemetry::Properties::new();
    for (key, count) in [
        ("digest_count", 2u64),
        ("report_count", 1),
        ("failed_count", 0),
    ] {
        expected.set(key, count.into());
    }
    assert_eq!(
        *recorded.lock().unwrap(),
        vec![(OBSERVER_SESSION_EVENT.to_string(), expected)]
    );
}

/// Each digest the observer reads bills the observed session: its reply
/// lands as an `observer` `model_usage` row.
#[tokio::test]
async fn each_digest_records_its_usage_on_the_observed_session() {
    let session_id = "observer-billed";
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_turn(billed_text_turn("NO_REPORT"));
    let (_store, mut bills) = bill_channel(session_id);
    let observer = feature(&provider, true);
    let context = context(session_id, 0);
    observer.on_session_start(&context, &[]);
    observer.on_message_end(&context, &user("hi"));
    observer.on_agent_end(&context);
    let bill = tokio::time::timeout(Duration::from_secs(5), bills.recv())
        .await
        .expect("the digest's bill landed")
        .unwrap();
    assert_eq!(
        serde_json::to_value(bill).unwrap(),
        expected_bill(session_id, "observer")
    );
}
