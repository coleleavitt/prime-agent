//! Scripted-loop verifier tests for the agent crate: each test replays a
//! scripted tool-call conversation through the loop via [`ScriptedProvider`],
//! checking normal turns, parallel tool calls, tool errors, mid-turn failures
//! with retry, abort, and max-iteration stops.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pa_agent::abort::AbortSignal;
use pa_agent::agent::{Agent, AgentOptions};
use pa_agent::agent_loop::{run_agent_loop_continue, AgentEventSink, AgentLoopConfig};
use pa_agent::scripted::ScriptedProvider;
use pa_agent::stream::AssistantMessageEvent;
use pa_agent::types::{
    AgentContext, AgentEvent, AgentMessage, AgentTool, AgentToolResult, AgentToolUpdateCallback,
    AssistantContent, AssistantMessage, Message, Model, StopReason, TextContent, ToolExecutionMode,
    ToolResultContent, ToolResultMessage, UserContent,
};

fn test_model() -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: String::new(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 100_000,
        max_tokens: 4_096,
        max_tokens_explicit: false,
    }
}

/// Echo tool: returns its `text` argument, optionally after a delay and/or
/// failing; records concurrent executions.
struct EchoTool {
    name: &'static str,
    delay_ms: u64,
    fail: bool,
    concurrent: AtomicUsize,
    max_concurrent: AtomicUsize,
    calls: AtomicUsize,
}

impl EchoTool {
    fn new(name: &'static str) -> Arc<Self> {
        Self::with_options(name, 0, false)
    }

    fn with_options(name: &'static str, delay_ms: u64, fail: bool) -> Arc<Self> {
        Arc::new(EchoTool {
            name,
            delay_ms,
            fail,
            concurrent: AtomicUsize::new(0),
            max_concurrent: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        })
    }
}

impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &'static str {
        "Echoes its text argument back."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
            })
        })
    }

    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: serde_json::Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let running = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_concurrent.fetch_max(running, Ordering::SeqCst);
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            self.concurrent.fetch_sub(1, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("boom from {}", self.name);
            }
            let text = params
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Ok(AgentToolResult::text(format!("echo:{text}")))
        })
    }
}

/// Tool that requests run termination via `terminate: true`.
#[derive(Default)]
struct TerminatingTool;

impl AgentTool for TerminatingTool {
    fn name(&self) -> &'static str {
        "stop_tool"
    }

    fn description(&self) -> &'static str {
        "Stops the agent run."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({ "type": "object", "properties": {} }))
    }

    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        _params: serde_json::Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let mut result = AgentToolResult::text("stopping");
            result.terminate = Some(true);
            Ok(result)
        })
    }
}

fn assistant_text(message: &AgentMessage) -> &AssistantMessage {
    match message {
        AgentMessage::Standard(Message::Assistant(assistant)) => assistant,
        other => panic!("expected assistant message, got {other:?}"),
    }
}

fn tool_result(message: &AgentMessage) -> &ToolResultMessage {
    match message {
        AgentMessage::Standard(Message::ToolResult(result)) => result,
        other => panic!("expected toolResult message, got {other:?}"),
    }
}

fn single_text(content: &[ToolResultContent]) -> &str {
    match content {
        [ToolResultContent::Text(TextContent { text, .. })] => text,
        other => panic!("expected single text block, got {other:?}"),
    }
}

fn event_type(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageUpdate { .. } => "message_update",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
    }
}

/// Build an agent wired to the scripted provider, with an event-type log.
async fn scripted_agent(
    tools: Vec<Arc<dyn AgentTool>>,
) -> (Agent, Arc<ScriptedProvider>, Arc<Mutex<Vec<&'static str>>>) {
    scripted_agent_with(tools, None).await
}

async fn scripted_agent_with(
    tools: Vec<Arc<dyn AgentTool>>,
    tool_execution: Option<ToolExecutionMode>,
) -> (Agent, Arc<ScriptedProvider>, Arc<Mutex<Vec<&'static str>>>) {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(AgentOptions {
        initial_state: pa_agent::agent::AgentInitialState {
            tools: Some(tools),
            ..Default::default()
        },
        stream_fn: Some(provider.stream_fn()),
        tool_execution,
        ..Default::default()
    });
    agent.set_model(test_model()).await;
    agent
        .subscribe({
            let events = Arc::clone(&events);
            move |event, _signal| {
                let events = Arc::clone(&events);
                Box::pin(async move {
                    events.lock().unwrap().push(event_type(&event));
                    Ok(())
                })
            }
        })
        .await;
    (agent, provider, events)
}

#[tokio::test]
async fn normal_turn_streams_and_completes() {
    let (agent, provider, events) = scripted_agent(vec![]).await;
    provider.push_text_turn("hello world");

    agent.prompt("hi").await.unwrap();
    agent.wait_for_idle().await;

    let state = agent.state().await;
    assert_eq!(state.messages.len(), 2);
    assert!(matches!(
        &state.messages[0],
        AgentMessage::Standard(Message::User(_))
    ));
    let assistant = assistant_text(&state.messages[1]);
    match &assistant.content[..] {
        [AssistantContent::Text(TextContent { text, .. })] => assert_eq!(text, "hello world"),
        other => panic!("expected single text block, got {other:?}"),
    }
    assert_eq!(assistant.stop_reason, StopReason::Stop);
    assert!(state.error_message.is_none());
    assert!(!state.is_streaming);

    let log: Vec<&str> = events.lock().unwrap().clone();
    assert_eq!(
        log,
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_update",
            "message_update",
            "message_update",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
        ],
        "event order must match the TS loop for a normal turn"
    );
}

#[tokio::test]
async fn tool_call_turn_dispatches_and_feeds_result_back() {
    let echo = EchoTool::new("echo");
    let (agent, provider, _events) = scripted_agent(vec![echo.clone()]).await;
    provider.push_tool_call_turn(
        Some("calling the tool"),
        vec![("call-1", "echo", serde_json::json!({ "text": "hi" }))],
    );
    provider.push_text_turn("done");

    agent.prompt("use the tool").await.unwrap();
    agent.wait_for_idle().await;

    assert_eq!(echo.calls.load(Ordering::SeqCst), 1);

    let calls = provider.calls();
    assert_eq!(calls.len(), 2);
    let second = &calls[1];
    assert_eq!(second.messages.len(), 3);
    assert!(matches!(&second.messages[0], Message::User(_)));
    assert!(matches!(&second.messages[1], Message::Assistant(_)));
    let result = match &second.messages[2] {
        Message::ToolResult(result) => result,
        other => panic!("expected toolResult, got {other:?}"),
    };
    assert_eq!(result.tool_call_id, "call-1");
    assert!(!result.is_error);
    assert_eq!(single_text(&result.content), "echo:hi");

    let state = agent.state().await;
    // user, assistant(toolUse), toolResult, assistant(final text)
    assert_eq!(state.messages.len(), 4);
    assert_eq!(
        assistant_text(&state.messages[3]).stop_reason,
        StopReason::Stop
    );
}

#[tokio::test]
async fn parallel_tool_calls_execute_concurrently_and_emit_in_order() {
    // Both calls target the same slow tool: overlap shows up as its
    // concurrent-execution high-water mark.
    let slow = EchoTool::with_options("echo", 150, false);
    let (agent, provider, _events) = scripted_agent(vec![slow.clone()]).await;
    provider.push_tool_call_turn(
        None,
        vec![
            ("call-a", "echo", serde_json::json!({ "text": "a" })),
            ("call-b", "echo", serde_json::json!({ "text": "b" })),
        ],
    );
    provider.push_text_turn("all done");

    agent.prompt("go").await.unwrap();
    agent.wait_for_idle().await;

    assert!(
        slow.max_concurrent.load(Ordering::SeqCst) >= 2,
        "parallel tool calls must overlap"
    );

    // Tool-result messages are appended in assistant source order.
    let state = agent.state().await;
    assert_eq!(tool_result(&state.messages[2]).tool_call_id, "call-a");
    assert_eq!(tool_result(&state.messages[3]).tool_call_id, "call-b");
}

#[tokio::test]
async fn sequential_tool_calls_run_one_at_a_time() {
    let tool = EchoTool::with_options("echo_seq", 60, false);
    let (agent, provider, _events) =
        scripted_agent_with(vec![tool.clone()], Some(ToolExecutionMode::Sequential)).await;
    provider.push_tool_call_turn(
        None,
        vec![
            ("call-1", "echo_seq", serde_json::json!({ "text": "1" })),
            ("call-2", "echo_seq", serde_json::json!({ "text": "2" })),
        ],
    );
    provider.push_text_turn("done");

    agent.prompt("go").await.unwrap();
    agent.wait_for_idle().await;

    assert_eq!(
        tool.max_concurrent.load(Ordering::SeqCst),
        1,
        "sequential mode must never overlap executions"
    );
}

#[tokio::test]
async fn tool_error_produces_error_tool_result_and_loop_continues() {
    let failing = EchoTool::with_options("bad_tool", 0, true);
    let (agent, provider, _events) = scripted_agent(vec![failing.clone()]).await;
    provider.push_tool_call_turn(
        None,
        vec![("call-1", "bad_tool", serde_json::json!({ "text": "x" }))],
    );
    provider.push_text_turn("recovered");

    agent.prompt("try the tool").await.unwrap();
    agent.wait_for_idle().await;

    assert_eq!(failing.calls.load(Ordering::SeqCst), 1);
    let state = agent.state().await;
    let result = tool_result(&state.messages[2]);
    assert!(result.is_error);
    assert_eq!(single_text(&result.content), "boom from bad_tool");
    assert_eq!(state.messages.len(), 4);
    assert!(matches!(
        &state.messages[3],
        AgentMessage::Standard(Message::Assistant(_))
    ));
}

#[tokio::test]
async fn unknown_tool_name_yields_error_tool_result() {
    let echo = EchoTool::new("echo");
    let (agent, provider, _events) = scripted_agent(vec![echo.clone()]).await;
    provider.push_tool_call_turn(
        None,
        vec![("call-1", "nonexistent", serde_json::json!({ "text": "x" }))],
    );
    provider.push_text_turn("ok");

    agent.prompt("go").await.unwrap();
    agent.wait_for_idle().await;

    assert_eq!(echo.calls.load(Ordering::SeqCst), 0);
    let state = agent.state().await;
    let result = tool_result(&state.messages[2]);
    assert!(result.is_error);
    assert_eq!(single_text(&result.content), "Tool nonexistent not found");
}

#[tokio::test]
async fn provider_stream_failure_mid_turn_ends_run_and_retry_continues() {
    let (agent, provider, _events) = scripted_agent(vec![]).await;
    provider.push_stream_failure_turn("partial answer", "connection reset by peer");

    agent.prompt("hello").await.unwrap();
    agent.wait_for_idle().await;

    let state = agent.state().await;
    assert_eq!(state.messages.len(), 2);
    let failed = assistant_text(&state.messages[1]);
    assert_eq!(failed.stop_reason, StopReason::Error);
    assert_eq!(
        failed.error_message.as_deref(),
        Some("connection reset by peer")
    );
    assert!(
        matches!(&failed.content[..], [AssistantContent::Text(TextContent { text, .. })] if text == "partial answer")
    );
    assert_eq!(
        state.error_message.as_deref(),
        Some("connection reset by peer")
    );

    // Retry via `runAgentLoopContinue`: the context still ends in a user
    // message, so the retry produces a fresh answer.
    let retry_provider = Arc::new(ScriptedProvider::new(test_model()));
    retry_provider.push_text_turn("retried answer");
    let config = AgentLoopConfig::new(test_model(), AgentLoopConfig::default_convert_to_llm());
    let context = AgentContext {
        system_prompt: String::new(),
        tools: Vec::new(),
        messages: vec![AgentMessage::user("hello")],
    };
    let retry_events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let emit: AgentEventSink = {
        let retry_events = Arc::clone(&retry_events);
        Arc::new(move |event| {
            let retry_events = Arc::clone(&retry_events);
            Box::pin(async move {
                retry_events.lock().unwrap().push(event_type(&event));
                Ok(())
            })
        })
    };
    let stream_fn = retry_provider.stream_fn();
    let messages = run_agent_loop_continue(context, &config, emit, None, Some(&stream_fn))
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    let retried = assistant_text(&messages[0]);
    assert_eq!(retried.stop_reason, StopReason::Stop);
    assert!(
        matches!(&retried.content[..], [AssistantContent::Text(TextContent { text, .. })] if text == "retried answer")
    );
    let log: Vec<&str> = retry_events.lock().unwrap().clone();
    assert_eq!(
        log,
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_update",
            "message_update",
            "message_update",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
}

#[tokio::test]
async fn user_abort_mid_stream_finalizes_aborted_assistant_message() {
    let (agent, provider, _events) = scripted_agent(vec![]).await;
    // No run is active yet: the abort reports it touched nothing.
    assert!(!agent.abort(), "an idle agent has no run to abort");
    // The provider streams partial text and then stalls; only an abort ends it.
    provider.push_stalled_turn("partial before abort");

    let prompt_task = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("hello").await }
    });

    // Wait until the partial text has been streamed, then abort like a user.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(agent.abort(), "the streaming run was active at the abort");

    prompt_task.await.unwrap().unwrap();
    agent.wait_for_idle().await;
    assert!(!agent.abort(), "the settled run is no longer active");

    let state = agent.state().await;
    assert_eq!(state.messages.len(), 2);
    let aborted = assistant_text(&state.messages[1]);
    assert_eq!(aborted.stop_reason, StopReason::Aborted);
    assert_eq!(
        aborted.error_message.as_deref(),
        Some("Request was aborted")
    );
    assert!(
        matches!(&aborted.content[..], [AssistantContent::Text(TextContent { text, .. })] if text == "partial before abort")
    );
    assert!(!state.is_streaming);
    assert!(state.pending_tool_calls.is_empty());
}

#[tokio::test]
async fn abort_during_tool_execution_produces_aborted_tool_result() {
    let slow = EchoTool::with_options("slow_tool", 60_000, false);
    let (agent, provider, _events) = scripted_agent(vec![slow.clone()]).await;
    provider.push_tool_call_turn(
        None,
        vec![("call-1", "slow_tool", serde_json::json!({ "text": "x" }))],
    );

    let prompt_task = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("go").await }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    agent.abort();

    prompt_task.await.unwrap().unwrap();
    agent.wait_for_idle().await;

    let state = agent.state().await;
    // user, assistant(toolUse), toolResult(aborted) — the TS abort test
    // asserts exactly this shape: no further assistant turn.
    let result = tool_result(&state.messages[2]);
    assert!(result.is_error);
    assert_eq!(single_text(&result.content), "Tool execution aborted");
    assert_eq!(state.messages.len(), 3);
    assert!(state.pending_tool_calls.is_empty());
    assert!(!state.is_streaming);
}

/// Upstream #891: the state snapshot records when each in-flight tool call
/// started, so observers can report how long a call has been running; the
/// start times clear with the in-flight set.
#[tokio::test]
async fn in_flight_tool_calls_record_their_start_time() {
    let slow = EchoTool::with_options("slow_tool", 60_000, false);
    let (agent, provider, _events) = scripted_agent(vec![slow.clone()]).await;
    provider.push_tool_call_turn(
        None,
        vec![("call-1", "slow_tool", serde_json::json!({ "text": "x" }))],
    );
    let before = pa_agent::now_ms();
    let prompt_task = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("go").await }
    });
    // Observable readiness: the tool call is in flight.
    let state = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = agent.state().await;
            if !state.pending_tool_calls.is_empty() {
                return state;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the tool call starts");
    let after = pa_agent::now_ms();
    let started: Vec<&String> = state.pending_tool_call_started_at.keys().collect();
    assert_eq!(started, vec!["call-1"]);
    let at = state.pending_tool_call_started_at["call-1"];
    assert!(
        (before..=after).contains(&at),
        "{before} <= {at} <= {after}"
    );

    agent.abort();
    prompt_task.await.unwrap().unwrap();
    agent.wait_for_idle().await;
    let state = agent.state().await;
    assert!(state.pending_tool_calls.is_empty());
    assert!(state.pending_tool_call_started_at.is_empty());
}

#[tokio::test]
async fn max_iterations_stops_after_configured_turn_count() {
    let echo = EchoTool::new("echo");
    let (_agent, provider, _events) = scripted_agent(vec![echo.clone()]).await;
    for _ in 0..5 {
        provider.push_tool_call_turn(
            None,
            vec![("call", "echo", serde_json::json!({ "text": "x" }))],
        );
    }

    // Stop before the third turn: `shouldStopBeforeTurn` is evaluated at
    // several boundaries per turn in the TS loop, so the turn count is
    // tracked in `shouldStopAfterTurn` (once per completed turn) and the
    // before-turn hook only reads it.
    let turn_count = Arc::new(AtomicUsize::new(0));
    let count_turns = Arc::clone(&turn_count);
    let turn_count_snapshot = Arc::clone(&turn_count);
    let bounded = Agent::new(AgentOptions {
        initial_state: pa_agent::agent::AgentInitialState {
            tools: Some(vec![echo.clone()]),
            ..Default::default()
        },
        stream_fn: Some(provider.stream_fn()),
        should_stop_after_turn: Some(Arc::new(move |_ctx| {
            let count_turns = Arc::clone(&count_turns);
            Box::pin(async move {
                count_turns.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            })
        })),
        should_stop_before_turn: Some(Arc::new(move || {
            turn_count_snapshot.load(Ordering::SeqCst) >= 2
        })),
        ..Default::default()
    });
    bounded.set_model(test_model()).await;
    bounded.prompt("go").await.unwrap();
    bounded.wait_for_idle().await;

    let state = bounded.state().await;
    let assistant_turns = state
        .messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::Standard(Message::Assistant(_))))
        .count();
    assert_eq!(assistant_turns, 2);
    assert_eq!(echo.calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.calls().len(), 2);
}

#[tokio::test]
async fn steering_message_injects_before_next_turn() {
    // Slow enough that the mid-run steer (50ms in) lands while turn 1 is
    // still executing its tool call.
    let echo = EchoTool::with_options("echo", 120, false);
    let (agent, provider, _events) = scripted_agent(vec![echo.clone()]).await;
    provider.push_tool_call_turn(
        None,
        vec![("call-1", "echo", serde_json::json!({ "text": "a" }))],
    );
    provider.push_tool_call_turn(
        None,
        vec![("call-2", "echo", serde_json::json!({ "text": "b" }))],
    );
    provider.push_text_turn("finished");

    // `prompt` resolves when the whole run completes (TS parity), so the
    // steering message must be queued concurrently, mid-run.
    let prompt_task = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("start").await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    agent.steer(AgentMessage::user("steered"));
    prompt_task.await.unwrap().unwrap();
    agent.wait_for_idle().await;

    let calls = provider.calls();
    assert!(
        calls.len() >= 2,
        "steering message must start a follow-up turn"
    );
    let last = calls.last().unwrap();
    let user_texts: Vec<&str> = last
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::User(u) => match &u.content {
                UserContent::Text(t) => Some(t.as_str()),
                UserContent::Parts(_) => None,
            },
            _ => None,
        })
        .collect();
    assert!(user_texts.contains(&"steered"));
}

#[tokio::test]
async fn terminate_tool_result_stops_the_run() {
    let (agent, provider, _events) = scripted_agent(vec![Arc::new(TerminatingTool)]).await;
    provider.push_tool_call_turn(None, vec![("call-1", "stop_tool", serde_json::json!({}))]);

    agent.prompt("go").await.unwrap();
    agent.wait_for_idle().await;

    assert_eq!(provider.calls().len(), 1);
    let state = agent.state().await;
    assert_eq!(state.messages.len(), 3);
    assert!(!state.is_streaming);
}

#[tokio::test]
async fn prompt_while_processing_is_rejected_with_ts_message() {
    let (agent, provider, _events) = scripted_agent(vec![]).await;
    provider.push_stalled_turn("streaming");

    let first = tokio::spawn({
        let agent = agent.clone();
        async move { agent.prompt("first").await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let error = agent.prompt("second").await.unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("Agent is already processing a prompt"),
        "unexpected error: {error:#}"
    );
    agent.abort();
    first.await.unwrap().unwrap();
}

#[test]
fn scripted_event_shapes_round_trip_through_the_event_enum() {
    // Terminal events must expose their message and deltas must not.
    let model = test_model();
    let mut partial = AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pa_agent::types::Usage::zero(),
        stop_reason: StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
        discarded_usage: None,
    };
    let event = AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "hi".into(),
        partial: partial.clone(),
    };
    assert!(event.is_delta());
    assert!(event.terminal_message().is_none());
    partial.content.push(AssistantContent::Text(TextContent {
        text: "hi".into(),
        text_signature: None,
    }));
    let done = AssistantMessageEvent::Done {
        reason: StopReason::Stop,
        message: partial,
    };
    assert!(done.terminal_message().is_some());
    assert!(!done.is_delta());

    let steps = pa_agent::scripted::text_turn_steps(&model, "hi");
    assert!(steps
        .iter()
        .any(|s| matches!(s, pa_agent::scripted::ScriptStep::Event(event) if event.terminal_message().is_some())));
    let failure = pa_agent::scripted::stream_failure_steps(&model, "partial", "boom");
    assert!(failure.iter().any(|s| matches!(
        s,
        pa_agent::scripted::ScriptStep::Event(event)
            if matches!(**event, AssistantMessageEvent::Error { .. })
    )));
}

/// A text turn that ends at the output-token limit (`stopReason: length`).
fn length_turn(provider: &ScriptedProvider, text: &str) {
    let mut steps = pa_agent::scripted::text_turn_steps(&test_model(), text);
    if let Some(pa_agent::scripted::ScriptStep::Event(event)) = steps.last_mut() {
        if let AssistantMessageEvent::Done { reason, message } = &mut **event {
            *reason = StopReason::Length;
            message.stop_reason = StopReason::Length;
        }
    }
    provider.push_turn(pa_agent::scripted::ScriptedTurn::Events(steps));
}

/// The run's transcript as `(role, text)` rows.
fn transcript(messages: &[AgentMessage]) -> Vec<(&'static str, String)> {
    messages
        .iter()
        .map(|message| match message {
            AgentMessage::Standard(Message::User(user)) => (
                "user",
                match &user.content {
                    UserContent::Text(text) => text.clone(),
                    UserContent::Parts(parts) => parts
                        .iter()
                        .filter_map(|part| match part {
                            pa_agent::types::UserPart::Text(text) => Some(text.text.clone()),
                            pa_agent::types::UserPart::Image(_) => None,
                        })
                        .collect(),
                },
            ),
            AgentMessage::Standard(Message::Assistant(assistant)) => (
                "assistant",
                assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => ("other", String::new()),
        })
        .collect()
}

fn length_continuation_agent(provider: &Arc<ScriptedProvider>, max: u32) -> Agent {
    Agent::new(AgentOptions {
        stream_fn: Some(provider.stream_fn()),
        length_continuation: Some(pa_agent::agent_loop::LengthContinuation {
            max_continuations: max,
            message: Arc::new(|attempt, max| {
                AgentMessage::Standard(Message::User(pa_agent::types::UserMessage {
                    content: UserContent::Text(format!("continue {attempt}/{max}")),
                    timestamp: 0,
                }))
            }),
        }),
        ..Default::default()
    })
}

/// Upstream #969: a reply cut off at the output-token limit continues in a
/// follow-up turn of the same run, bounded by the policy; the bound ends
/// the run on the last truncated reply.
#[tokio::test]
async fn a_length_truncated_reply_auto_continues_up_to_the_bound() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    let agent = length_continuation_agent(&provider, 2);
    agent.set_model(test_model()).await;
    length_turn(&provider, "part one");
    length_turn(&provider, "part two");
    length_turn(&provider, "part three");
    provider.push_text_turn("never requested");
    agent.prompt("write it all").await.unwrap();
    agent.wait_for_idle().await;
    assert_eq!(
        transcript(&agent.state().await.messages),
        vec![
            ("user", "write it all".to_string()),
            ("assistant", "part one".to_string()),
            ("user", "continue 1/2".to_string()),
            ("assistant", "part two".to_string()),
            ("user", "continue 2/2".to_string()),
            ("assistant", "part three".to_string()),
        ]
    );
    assert_eq!(provider.calls().len(), 3);
}

/// The continuation stops on a natural completion, and a fresh prompt
/// starts a fresh bound.
#[tokio::test]
async fn a_length_continuation_ends_at_the_natural_stop_and_resets_per_run() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    let agent = length_continuation_agent(&provider, 1);
    agent.set_model(test_model()).await;
    length_turn(&provider, "cut");
    provider.push_text_turn("done");
    agent.prompt("first").await.unwrap();
    agent.wait_for_idle().await;
    length_turn(&provider, "cut again");
    provider.push_text_turn("done again");
    agent.prompt("second").await.unwrap();
    agent.wait_for_idle().await;
    assert_eq!(
        transcript(&agent.state().await.messages),
        vec![
            ("user", "first".to_string()),
            ("assistant", "cut".to_string()),
            ("user", "continue 1/1".to_string()),
            ("assistant", "done".to_string()),
            ("user", "second".to_string()),
            ("assistant", "cut again".to_string()),
            ("user", "continue 1/1".to_string()),
            ("assistant", "done again".to_string()),
        ]
    );
}

/// Without the policy (the default, TS v0.9.8 behavior) a truncated reply
/// ends the run.
#[tokio::test]
async fn a_length_truncated_reply_ends_the_run_without_the_policy() {
    let (agent, provider, _events) = scripted_agent(vec![]).await;
    length_turn(&provider, "cut");
    provider.push_text_turn("never requested");
    agent.prompt("go").await.unwrap();
    agent.wait_for_idle().await;
    assert_eq!(
        transcript(&agent.state().await.messages),
        vec![("user", "go".to_string()), ("assistant", "cut".to_string())]
    );
}

fn guarded_agent(provider: &Arc<ScriptedProvider>) -> Agent {
    Agent::new(AgentOptions {
        stream_fn: Some(provider.stream_fn()),
        repetition_guard: Some(pa_agent::repetition_guard::RepetitionGuardConfig {
            guard_text: true,
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Upstream #1798: a degenerate looping stream is stopped by the guard
/// instead of streaming to the cap: the reply settles as an error naming
/// the guard (`stopReasonRaw: repetition_loop`), trimmed to its first
/// repeats, and the run ends.
#[tokio::test]
async fn the_repetition_guard_stops_a_looping_stream() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    let agent = guarded_agent(&provider);
    agent.set_model(test_model()).await;
    provider.push_text_turn(&format!("Thinking it over: {}", "the ".repeat(5_000)));
    provider.push_text_turn("never requested");
    agent.prompt("go").await.unwrap();
    agent.wait_for_idle().await;
    let state = agent.state().await;
    let reply = assistant_text(state.messages.last().unwrap());
    // The guard fires at the first 256-byte check past the 2000-char span
    // floor: 507 repeats in, far short of the 5000 the stream would send.
    assert_eq!(
        (
            reply.stop_reason,
            reply.stop_reason_raw.as_deref(),
            reply.error_message.as_deref(),
        ),
        (
            StopReason::Error,
            Some("repetition_loop"),
            Some("Generation stopped by the repetition guard: the output repeated one 4-character unit 507 times in a row"),
        )
    );
    assert_eq!(
        transcript(&state.messages),
        vec![
            ("user", "go".to_string()),
            ("assistant", "Thinking it over: the the".to_string()),
        ]
    );
    assert_eq!(provider.calls().len(), 1);
}

/// The guard never stops real output with repeated structure, and without
/// the guard (the default) a loop streams to its end.
#[tokio::test]
async fn the_repetition_guard_passes_real_output_and_is_off_by_default() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    let agent = guarded_agent(&provider);
    agent.set_model(test_model()).await;
    let code = (0..120).fold(String::new(), |mut code, row| {
        use std::fmt::Write as _;
        let _ = write!(
            code,
            "    assert_eq!(table[{row}], expected[{row}]);\n    }}\n"
        );
        code
    });
    provider.push_text_turn(&code);
    agent.prompt("write the test").await.unwrap();
    agent.wait_for_idle().await;
    let reply = assistant_text(agent.state().await.messages.last().unwrap()).clone();
    assert_eq!(reply.stop_reason, StopReason::Stop);

    let (agent, provider, _events) = scripted_agent(vec![]).await;
    let looping = "the ".repeat(5_000);
    provider.push_text_turn(&looping);
    agent.prompt("go").await.unwrap();
    agent.wait_for_idle().await;
    let reply = assistant_text(agent.state().await.messages.last().unwrap()).clone();
    assert_eq!(reply.stop_reason, StopReason::Stop);
}

/// A text turn that ends with `stop_reason` and delivers no tool call.
fn undelivered_turn(
    provider: &ScriptedProvider,
    model: &Model,
    text: &str,
    stop_reason: StopReason,
) {
    let mut steps = pa_agent::scripted::text_turn_steps(model, text);
    if let Some(pa_agent::scripted::ScriptStep::Event(event)) = steps.last_mut() {
        if let AssistantMessageEvent::Done { reason, message } = &mut **event {
            *reason = stop_reason;
            message.stop_reason = stop_reason;
        }
    }
    provider.push_turn(pa_agent::scripted::ScriptedTurn::Events(steps));
}

fn completions_model() -> Model {
    Model {
        api: "openai-completions".into(),
        ..test_model()
    }
}

type ToolChoiceLog = Arc<Mutex<Vec<Option<pa_types::ai::RequestToolChoice>>>>;

/// What the installed recovery hook answers.
#[derive(Clone, Copy)]
enum RecoveryHook {
    Mint,
    Decline,
}

/// One ineligible case: the model, the context's tools, the finish, the hook.
type IneligibleCase = (Model, Vec<Arc<dyn AgentTool>>, StopReason, RecoveryHook);

/// An agent with the dropped-tool-call recovery installed (the hook mints
/// `recover` or declines), recording every request's tool choice.
async fn recovery_agent(
    model: Model,
    tools: Vec<Arc<dyn AgentTool>>,
    hook: RecoveryHook,
) -> (Agent, Arc<ScriptedProvider>, ToolChoiceLog) {
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    let choices: ToolChoiceLog = Arc::new(Mutex::new(Vec::new()));
    let inner = provider.stream_fn();
    let recorded = Arc::clone(&choices);
    let stream_fn: pa_agent::stream::StreamFn = Arc::new(move |model, context, options| {
        recorded.lock().unwrap().push(options.tool_choice);
        inner(model, context, options)
    });
    let agent = Agent::new(AgentOptions {
        initial_state: pa_agent::agent::AgentInitialState {
            tools: Some(tools),
            ..Default::default()
        },
        stream_fn: Some(stream_fn),
        ..Default::default()
    });
    agent.set_model(model).await;
    agent.set_tool_intent_recovery_hook(Some(Arc::new(move |_context| {
        Box::pin(async move {
            Ok(matches!(hook, RecoveryHook::Mint).then(|| {
                AgentMessage::Standard(Message::User(pa_agent::types::UserMessage {
                    content: UserContent::Text("recover".to_string()),
                    timestamp: 0,
                }))
            }))
        })
    })));
    (agent, provider, choices)
}

/// Upstream #2530: a reply that reports `toolUse` but delivers no call
/// retries once with a required tool choice; the next turn's call runs and
/// the run ends normally. A second undelivered call in a later run gets its
/// own single recovery, and a repeat inside that run ends the turn.
#[tokio::test]
async fn an_undelivered_tool_use_reply_retries_once_with_a_required_tool_choice() {
    let echo = EchoTool::new("echo");
    let tools: Vec<Arc<dyn AgentTool>> = vec![echo.clone()];
    let model = completions_model();
    let (agent, provider, choices) = recovery_agent(model.clone(), tools, RecoveryHook::Mint).await;
    undelivered_turn(
        &provider,
        &model,
        "Let me check the evidence.",
        StopReason::ToolUse,
    );
    provider.push_tool_call_turn(
        None,
        vec![("call-1", "echo", serde_json::json!({ "text": "x" }))],
    );
    provider.push_text_turn("done");
    agent.prompt("why do the children die?").await.unwrap();
    agent.wait_for_idle().await;
    assert_eq!(
        *choices.lock().unwrap(),
        vec![None, Some(pa_types::ai::RequestToolChoice::Required), None]
    );
    assert_eq!(echo.calls.load(Ordering::SeqCst), 1);

    undelivered_turn(
        &provider,
        &model,
        "I'll inspect the logs.",
        StopReason::ToolUse,
    );
    undelivered_turn(&provider, &model, "I'll check again.", StopReason::ToolUse);
    provider.push_text_turn("never requested");
    agent.prompt("inspect the logs too").await.unwrap();
    agent.wait_for_idle().await;
    let recoveries = transcript(&agent.state().await.messages)
        .into_iter()
        .filter(|row| *row == ("user", "recover".to_string()))
        .count();
    assert_eq!(recoveries, 2);
    assert_eq!(provider.calls().len(), 5);
}

/// A `length` finish with no call may be ordinary truncation: the retry
/// keeps the run's own (default) tool choice.
#[tokio::test]
async fn an_undelivered_length_reply_retries_with_the_runs_own_tool_choice() {
    let echo = EchoTool::new("echo");
    let model = completions_model();
    let (agent, provider, choices) =
        recovery_agent(model.clone(), vec![echo.clone()], RecoveryHook::Mint).await;
    undelivered_turn(&provider, &model, "", StopReason::Length);
    provider.push_text_turn("finished");
    agent.prompt("go").await.unwrap();
    agent.wait_for_idle().await;
    assert_eq!(*choices.lock().unwrap(), vec![None, None]);
    assert_eq!(
        transcript(&agent.state().await.messages),
        vec![
            ("user", "go".to_string()),
            ("assistant", String::new()),
            ("user", "recover".to_string()),
            ("assistant", "finished".to_string()),
        ]
    );
}

/// Ineligible terminal replies end the run without asking for a recovery:
/// a plain stop, a tool-less context, a non-Completions API, or a hook
/// that declines.
#[tokio::test]
async fn ineligible_terminal_replies_end_the_run_without_a_retry() {
    let completions = completions_model();
    let cases: Vec<IneligibleCase> = vec![
        (
            completions.clone(),
            vec![EchoTool::new("echo")],
            StopReason::Stop,
            RecoveryHook::Mint,
        ),
        (
            completions.clone(),
            vec![],
            StopReason::ToolUse,
            RecoveryHook::Mint,
        ),
        (
            test_model(),
            vec![EchoTool::new("echo")],
            StopReason::ToolUse,
            RecoveryHook::Mint,
        ),
        (
            completions.clone(),
            vec![EchoTool::new("echo")],
            StopReason::ToolUse,
            RecoveryHook::Decline,
        ),
    ];
    let mut served = Vec::new();
    for (model, tools, stop_reason, hook) in cases {
        let (agent, provider, _choices) = recovery_agent(model.clone(), tools, hook).await;
        undelivered_turn(&provider, &model, "Let me check the logs.", stop_reason);
        provider.push_text_turn("unexpected retry");
        agent.prompt("Inspect the logs.").await.unwrap();
        agent.wait_for_idle().await;
        served.push(provider.calls().len());
    }
    assert_eq!(served, vec![1, 1, 1, 1]);
}

/// A tool-call response whose stream ends at the output-token limit
/// (`stopReason: length`, `output` tokens spent), as Anthropic ends a
/// response cut off while it writes a call's arguments.
fn cut_off_tool_call_turn(
    provider: &ScriptedProvider,
    model: &Model,
    calls: Vec<(&str, &str, serde_json::Value)>,
    output: u64,
) {
    let mut steps = pa_agent::scripted::tool_call_turn_steps(model, None, calls);
    if let Some(pa_agent::scripted::ScriptStep::Event(event)) = steps.last_mut() {
        if let AssistantMessageEvent::Done { reason, message } = &mut **event {
            *reason = StopReason::Length;
            message.stop_reason = StopReason::Length;
            message.usage.output = output;
        }
    }
    provider.push_turn(pa_agent::scripted::ScriptedTurn::Events(steps));
}

/// The tool result the model reads for a call cut off at the 16384-token
/// output limit.
const CUT_OFF_ECHO_NOTICE: &str = "This echo call was not run: the response reached the output \
    limit of 16384 tokens while the call's arguments were still being written, so they were cut \
    off. Thinking and text count toward the limit. Split the work into smaller steps, for \
    example several shorter code cells, or a large file written in parts with one call per part.";

/// A response cut off at the output limit while it wrote its last tool
/// call (the arguments stream incomplete; here they parse as `{}`) does not
/// run that call: the model reads why as the call's error result and the
/// run goes on. Neither the dropped-tool-call retry (upstream #2530) nor
/// the length auto-continuation (#969) fires on top: the reply delivered a
/// call, and its result drives the next turn.
#[tokio::test]
async fn a_tool_call_cut_off_at_the_output_limit_is_not_run_and_the_model_hears_why() {
    let echo = EchoTool::new("echo");
    let model = completions_model();
    let provider = Arc::new(ScriptedProvider::new(model.clone()));
    let agent = Agent::new(AgentOptions {
        initial_state: pa_agent::agent::AgentInitialState {
            tools: Some(vec![echo.clone()]),
            ..Default::default()
        },
        stream_fn: Some(provider.stream_fn()),
        length_continuation: Some(pa_agent::agent_loop::LengthContinuation {
            max_continuations: 3,
            message: Arc::new(|attempt, max| {
                AgentMessage::Standard(Message::User(pa_agent::types::UserMessage {
                    content: UserContent::Text(format!("continue {attempt}/{max}")),
                    timestamp: 0,
                }))
            }),
        }),
        ..Default::default()
    });
    agent.set_model(model.clone()).await;
    agent.set_tool_intent_recovery_hook(Some(Arc::new(|_context| {
        Box::pin(async {
            Ok(Some(AgentMessage::Standard(Message::User(
                pa_agent::types::UserMessage {
                    content: UserContent::Text("recover".to_string()),
                    timestamp: 0,
                },
            ))))
        })
    })));
    cut_off_tool_call_turn(
        &provider,
        &model,
        vec![("call-1", "echo", serde_json::json!({}))],
        16_384,
    );
    provider.push_text_turn("I'll split it into smaller cells.");
    agent.prompt("write the module").await.unwrap();
    agent.wait_for_idle().await;

    assert_eq!(echo.calls.load(Ordering::SeqCst), 0);
    let calls = provider.calls();
    assert_eq!(calls.len(), 2);
    let seen: Vec<(String, bool, String)> = calls[1]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some((
                result.tool_call_id.clone(),
                result.is_error,
                single_text(&result.content).to_string(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        seen,
        vec![("call-1".to_string(), true, CUT_OFF_ECHO_NOTICE.to_string())]
    );
    assert_eq!(
        transcript(&agent.state().await.messages),
        vec![
            ("user", "write the module".to_string()),
            ("assistant", String::new()),
            ("other", String::new()),
            ("assistant", "I'll split it into smaller cells.".to_string()),
        ]
    );
}

/// Calls the model finished before the cut-off one are complete (each
/// block ends before the next starts), so they run as usual; only the last
/// block, the call the limit interrupted, is held back, even when its
/// partial arguments happen to pass the tool's schema.
#[tokio::test]
async fn complete_calls_before_the_cut_off_one_still_run() {
    let echo = EchoTool::new("echo");
    let (agent, provider, _events) = scripted_agent(vec![echo.clone()]).await;
    cut_off_tool_call_turn(
        &provider,
        &test_model(),
        vec![
            ("call-a", "echo", serde_json::json!({ "text": "a" })),
            ("call-b", "echo", serde_json::json!({ "text": "par" })),
        ],
        16_384,
    );
    provider.push_text_turn("done");
    agent.prompt("go").await.unwrap();
    agent.wait_for_idle().await;

    assert_eq!(echo.calls.load(Ordering::SeqCst), 1);
    let state = agent.state().await;
    let results: Vec<(String, bool, String)> = state
        .messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Standard(Message::ToolResult(result)) => Some((
                result.tool_call_id.clone(),
                result.is_error,
                single_text(&result.content).to_string(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![
            ("call-a".to_string(), false, "echo:a".to_string()),
            ("call-b".to_string(), true, CUT_OFF_ECHO_NOTICE.to_string()),
        ]
    );
    assert_eq!(
        assistant_text(state.messages.last().unwrap()).stop_reason,
        StopReason::Stop
    );
}

/// A length stop that came after the call finished (text follows it) cut
/// the text, not the call: the call runs.
#[tokio::test]
async fn a_call_followed_by_cut_off_text_still_runs() {
    let echo = EchoTool::new("echo");
    let (agent, provider, _events) = scripted_agent(vec![echo.clone()]).await;
    let model = test_model();
    let mut steps = pa_agent::scripted::tool_call_turn_steps(
        &model,
        None,
        vec![("call-1", "echo", serde_json::json!({ "text": "x" }))],
    );
    if let Some(pa_agent::scripted::ScriptStep::Event(event)) = steps.last_mut() {
        if let AssistantMessageEvent::Done { reason, message } = &mut **event {
            *reason = StopReason::Length;
            message.stop_reason = StopReason::Length;
            message.content.push(
                serde_json::from_value(
                    serde_json::json!({ "type": "text", "text": "And then I will" }),
                )
                .expect("a text block"),
            );
        }
    }
    provider.push_turn(pa_agent::scripted::ScriptedTurn::Events(steps));
    provider.push_text_turn("done");
    agent.prompt("go").await.unwrap();
    agent.wait_for_idle().await;

    assert_eq!(echo.calls.load(Ordering::SeqCst), 1);
}
