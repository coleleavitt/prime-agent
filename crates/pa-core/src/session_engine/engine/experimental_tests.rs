//! The `experimental` behaviors through a real session engine: the
//! usage-limit wrap-up row.

use std::collections::BTreeMap;

use pa_agent::scripted::ScriptedProvider;

use super::*;
use crate::session_engine::tool_bridge::bridge_tool;
use crate::tools::tool_definition::{ExecutionMode, ToolDefinition, ToolExecutionResult};

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

fn noop_tool() -> ToolDefinition {
    ToolDefinition {
        name: "noop".to_string(),
        label: "Noop".to_string(),
        description: "Does nothing".to_string(),
        prompt_snippet: String::new(),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
        execution_mode: Some(ExecutionMode::Sequential),
        prepare_arguments: None,
        execute: Arc::new(|_id, _params, _signal, _on_update| {
            Box::pin(async move { Ok(ToolExecutionResult::text("done".to_string())) })
        }),
    }
}

/// A provider that reports `headers` on every response, the way pa-ai calls
/// the request's response hook.
fn reporting_stream_fn(
    provider: &Arc<ScriptedProvider>,
    headers: BTreeMap<String, String>,
) -> pa_agent::stream::StreamFn {
    let inner = provider.stream_fn();
    Arc::new(move |model, context, options| {
        if let Some(hook) = options.on_response.as_ref() {
            hook(
                pa_agent::stream::ProviderResponse {
                    status: 200,
                    headers: headers.clone(),
                },
                &model,
            );
        }
        inner(model, context, options)
    })
}

fn user_texts(call: &pa_agent::stream::LlmContext) -> Vec<String> {
    call.messages
        .iter()
        .filter_map(|message| match message {
            pa_agent::types::Message::User(user) => Some(match &user.content {
                pa_agent::types::UserContent::Text(text) => text.clone(),
                pa_agent::types::UserContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|part| match part {
                        pa_agent::types::UserPart::Text(text) => Some(text.text.clone()),
                        pa_agent::types::UserPart::Image(_) => None,
                    })
                    .collect(),
            }),
            _ => None,
        })
        .collect()
}

async fn run_tool_loop(settings: &str) -> Vec<pa_agent::stream::LlmContext> {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_tool_call_turn(None, vec![("call-1", "noop", serde_json::json!({}))]);
    provider.push_text_turn("checkpointed");
    let tmp = crate::test_support::ThreadTempDir::new();
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("settings.json"), settings).unwrap();
    let headers: BTreeMap<String, String> = [
        ("anthropic-ratelimit-unified-5h-utilization", "0.97"),
        ("anthropic-ratelimit-unified-5h-reset", "99999999999"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .collect();
    let engine = create_session(SessionEngineConfig {
        cwd: tmp.path().to_path_buf(),
        agent_dir,
        model: Some(test_model()),
        stream_fn: Some(reporting_stream_fn(&provider, headers)),
        tools: vec![bridge_tool(noop_tool())],
        ..Default::default()
    })
    .await
    .unwrap();
    engine
        .session
        .prompt("work", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    provider.calls()
}

/// Near the five-hour limit, the request after a tool batch carries Claude
/// Code's checkpoint row; without `usageLimitWrapUp` it never does.
#[tokio::test]
async fn the_usage_limit_wrap_up_rides_the_request_after_a_tool_batch() {
    let calls = run_tool_loop(r#"{"experimental": {"usageLimitWrapUp": true}}"#).await;
    assert_eq!(calls.len(), 2);
    let wrap_up = crate::session_engine::usage_limit::NEAR_LIMIT_WRAP_UP_TEXT.to_string();
    assert!(!user_texts(&calls[0]).contains(&wrap_up));
    assert!(
        user_texts(&calls[1]).contains(&wrap_up),
        "{:?}",
        user_texts(&calls[1])
    );

    let calls = run_tool_loop("{}").await;
    assert_eq!(calls.len(), 2);
    assert!(!user_texts(&calls[1]).contains(&wrap_up));
}

/// With `goalEvaluator`, a natural stop under an active goal asks the
/// evaluator: not met continues the same run with the reason, met
/// completes the goal and lets the agent stop.
#[tokio::test]
async fn the_goal_evaluator_keeps_the_run_going_until_the_goal_is_met() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn("I think it is done");
    provider.push_text_turn(r#"{"ok": false, "reason": "the tests were never run"}"#);
    provider.push_text_turn("ran the tests: 3 passed");
    provider.push_text_turn(r#"{"ok": true, "reason": "\"3 passed\""}"#);
    let tmp = crate::test_support::ThreadTempDir::new();
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"experimental": {"goalEvaluator": true}}"#,
    )
    .unwrap();
    let engine = create_session(SessionEngineConfig {
        cwd: tmp.path().to_path_buf(),
        agent_dir,
        model: Some(test_model()),
        stream_fn: Some(provider.stream_fn()),
        tools: Vec::new(),
        ..Default::default()
    })
    .await
    .unwrap();
    {
        let persistence = engine.session.shared_persistence();
        let mut driver = engine.goal_driver.lock().await;
        let mut session = persistence.lock().await;
        driver.start(&mut session, "the tests pass", None).unwrap();
    }
    engine
        .session
        .prompt("finish it", crate::session_engine::PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;
    let calls = provider.calls();
    assert_eq!(calls.len(), 4);
    let evaluator_prompt = crate::session_engine::goal_evaluator::GOAL_EVALUATOR_SYSTEM_PROMPT;
    assert_eq!(calls[1].system_prompt.as_deref(), Some(evaluator_prompt));
    assert_eq!(calls[3].system_prompt.as_deref(), Some(evaluator_prompt));
    assert_ne!(calls[2].system_prompt.as_deref(), Some(evaluator_prompt));
    let not_met = crate::session_engine::goal_evaluator::not_met_text(
        "the tests pass",
        "the tests were never run",
    );
    assert_eq!(user_texts(&calls[2]).last(), Some(&not_met));
    assert_eq!(
        engine.goal_driver.lock().await.state().status,
        crate::goals::GoalStatus::Complete
    );
}
