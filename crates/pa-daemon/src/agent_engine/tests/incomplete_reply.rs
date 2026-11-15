//! A final reply that cannot end its turn (no visible text where an answer
//! is owed, or cut off at the output limit) gets one retry through the
//! provider retry driver, then surfaces as the turn's error.

use super::*;

/// The assistant rows one run settled.
fn assistant_rows(events: &[EngineEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::AssistantMessage(message) => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// One reply's joined text.
fn text_of(message: &serde_json::Value) -> String {
    message["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// The `(retry starts, retry end successes)` one run emitted.
fn retry_events(events: &[EngineEvent]) -> (usize, Vec<bool>) {
    let starts = events
        .iter()
        .filter(|event| matches!(event, EngineEvent::AutoRetryStart { .. }))
        .count();
    let ends = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::AutoRetryEnd { success, .. } => Some(*success),
            _ => None,
        })
        .collect();
    (starts, ends)
}

fn thinking_only_stop() -> serde_json::Value {
    json!({
        "content": [{"type": "thinking", "thinking": "The answer is a short analogy."}],
        "stopReason": "stop",
    })
}

fn cut_off_while_thinking() -> serde_json::Value {
    json!({
        "content": [{"type": "thinking", "thinking": "Re-deriving the same numbers once more"}],
        "stopReason": "length",
    })
}

/// Run one request through a fresh faux engine.
fn run_request(
    script: &serde_json::Value,
    message: &str,
    custom_message: Option<serde_json::Value>,
) -> Vec<EngineEvent> {
    let _faux = FAUX_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::TempDir::new().unwrap();
    // The fork's output-limit recovery (on by default) continues a cut-off
    // reply inside the loop before it can settle; off, the cut-off reply
    // reaches the retry driver this file covers.
    std::fs::create_dir_all(dir.path().join("agent")).unwrap();
    std::fs::write(
        dir.path().join("agent/settings.json"),
        r#"{"lengthContinuations": 0}"#,
    )
    .unwrap();
    let engine = AgentSessionEngine::new(AgentEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: dir.path().join("agent"),
        provider: None,
        model: None,
        api_key: None,
        thinking: None,
        session_dir: None,
        session_file: None,
        faux_script: Some(script.to_string()),
        supervisor_link: None,
        telemetry_disabled: None,
        cron_store: None,
        queued_steering_probe: None,
        steering_fold: None,
    })
    .unwrap();
    let engine = std::sync::Arc::new(engine);
    engine.register_arc();
    let mut events = Vec::new();
    engine.run_prompt(
        0,
        PromptRequest {
            batch: Vec::new(),
            images: Vec::new(),
            message: message.to_string(),
            source: "user".to_string(),
            agent_message_id: None,
            custom_message,
        },
        &|| false,
        &mut |event| {
            events.push(event);
            true
        },
    );
    events
}

fn agent_message_row(text: &str) -> serde_json::Value {
    json!({
        "role": "custom",
        "customType": "agent_message",
        "content": text,
        "display": true,
        "timestamp": 0
    })
}

// A silent reply (no visible text, no tool call, a normal stop) never
// reaches the retry driver in the fork: the agent loop's empty-turn retry
// (pa-agent, upstream #1896) re-requests it first and carries the discarded
// attempt's spend as `discardedUsage`.
#[test]
fn a_silent_reply_to_a_user_prompt_is_re_requested_by_the_loop() {
    let events = run_request(
        &json!({"engine": "faux", "responses": [thinking_only_stop(), {"text": "The answer, as text."}]}),
        "explain the slow query",
        None,
    );
    let assistants = assistant_rows(&events);
    assert_eq!(assistants.len(), 1, "the silent attempt is never settled");
    assert_eq!(text_of(&assistants[0]), "The answer, as text.");
    assert_eq!(
        assistants[0]["discardedUsage"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(retry_events(&events), (0, vec![]));
    assert_eq!(events.last(), Some(&EngineEvent::Done(Ok(()))));
}

#[test]
fn a_reply_cut_off_at_the_output_limit_retries_on_any_turn() {
    let events = run_request(
        &json!({"engine": "faux", "responses": [cut_off_while_thinking(), {"text": "Done: the file is saved."}]}),
        "continue the design task",
        Some(agent_message_row("continue the design task")),
    );
    let assistants = assistant_rows(&events);
    assert_eq!(assistants.len(), 2);
    assert_eq!(text_of(&assistants[1]), "Done: the file is saved.");
    assert_eq!(retry_events(&events), (1, vec![true]));
    assert_eq!(events.last(), Some(&EngineEvent::Done(Ok(()))));
}

#[test]
fn a_retry_after_tool_work_resumes_without_rerunning_the_tool() {
    let events = run_request(
        &json!({"engine": "faux", "responses": [
            {"content": [{"type": "toolCall", "id": "call-1", "name": "synthetic_missing_tool", "arguments": {}}]},
            cut_off_while_thinking(),
            {"text": "The task is complete."}
        ]}),
        "run the task",
        None,
    );
    let tool_runs = events
        .iter()
        .filter(|event| matches!(event, EngineEvent::ToolExecutionStart { .. }))
        .count();
    assert_eq!(tool_runs, 1, "the retry re-issues only the failed reply");
    assert_eq!(
        text_of(assistant_rows(&events).last().unwrap()),
        "The task is complete."
    );
    assert_eq!(retry_events(&events), (1, vec![true]));
    assert_eq!(events.last(), Some(&EngineEvent::Done(Ok(()))));
}

#[test]
fn a_visible_answer_ends_the_turn_without_a_retry() {
    let events = run_request(
        &json!({"engine": "faux", "responses": [
            {"content": [{"type": "thinking", "thinking": "short"}, {"type": "text", "text": "4"}], "stopReason": "stop"}
        ]}),
        "2+2?",
        None,
    );
    assert_eq!(assistant_rows(&events).len(), 1);
    assert_eq!(retry_events(&events), (0, vec![]));
    assert_eq!(events.last(), Some(&EngineEvent::Done(Ok(()))));
}
