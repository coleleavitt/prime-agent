//! The observer feature end to end over a scripted observer model.

use std::time::Duration;

use pa_agent::scripted::ScriptedProvider;

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
