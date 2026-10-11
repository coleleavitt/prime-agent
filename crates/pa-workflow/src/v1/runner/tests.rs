//! The one-turn boundary battery (TS `run-workflow-agent.test.ts`): the
//! closed result, one physical request with the request's own credential,
//! the tool-call rejection, the size bound, and the cancellation races.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use pa_agent::stream::{AssistantMessageEvent, LlmContext, ModelStream, StreamFn};
use pa_agent::types::{
    AssistantContent,
    AssistantMessage,
    Model,
    StopReason,
    TextContent,
    ToolCall,
    Usage as AgentUsage,
    UsageCost,
};
use tokio::sync::Notify;

use super::*;

fn model() -> Model {
    serde_json::from_value(serde_json::json!({
        "id": "m1", "name": "M1", "api": "test", "provider": "p1",
        "baseUrl": "http://localhost", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .unwrap()
}

fn message(
    content: Vec<AssistantContent>,
    stop_reason: StopReason,
    usage: AgentUsage,
) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "test".to_string(),
        provider: "p1".to_string(),
        model: "m1".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage,
        stop_reason,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
        discarded_usage: None,
    }
}

fn text(text: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.to_string(),
        text_signature: None,
    })
}

fn usage(total: u64, cost_total: f64) -> AgentUsage {
    AgentUsage {
        input: 2,
        output: 3,
        cache_read: 4,
        cache_write: 5,
        total_tokens: total,
        cost: UsageCost {
            input: 1.0,
            output: 2.0,
            cache_read: 3.0,
            cache_write: 4.0,
            total: cost_total,
        },
    }
}

fn done(message: AssistantMessage) -> AssistantMessageEvent {
    AssistantMessageEvent::Done {
        reason: message.stop_reason,
        message,
    }
}

/// What a scripted stream does once its events run out.
#[derive(Clone, Copy)]
enum Then {
    /// End; the result is the last terminal message.
    End,
    /// Never end, even when closed (a stuck transport).
    Stall,
    /// Wait for `close`, then settle with an aborted terminal.
    AbortOnClose,
}

struct TestStream {
    events: VecDeque<AssistantMessageEvent>,
    then: Then,
    closed: Arc<Notify>,
    is_closed: bool,
    result: Option<AssistantMessage>,
}

impl ModelStream for TestStream {
    fn next_event(&mut self) -> pa_agent::BoxFut<'_, Option<AssistantMessageEvent>> {
        Box::pin(async {
            if let Some(event) = self.events.pop_front() {
                if let Some(message) = event.terminal_message() {
                    self.result = Some(message.clone());
                }
                return Some(event);
            }
            match self.then {
                Then::End => None,
                Then::Stall => std::future::pending().await,
                Then::AbortOnClose => {
                    if self.result.is_some() {
                        return None;
                    }
                    if !self.is_closed {
                        self.closed.notified().await;
                    }
                    let aborted = message(Vec::new(), StopReason::Aborted, AgentUsage::zero());
                    self.result = Some(aborted.clone());
                    Some(AssistantMessageEvent::Error {
                        reason: StopReason::Aborted,
                        error: aborted,
                    })
                }
            }
        })
    }

    fn result(&mut self) -> pa_agent::BoxFut<'_, anyhow::Result<AssistantMessage>> {
        Box::pin(async {
            match (&self.result, self.then) {
                (Some(message), _) => Ok(message.clone()),
                (None, Then::Stall) => std::future::pending().await,
                (None, Then::End | Then::AbortOnClose) => {
                    anyhow::bail!("stream ended without a result")
                }
            }
        })
    }

    fn close(&mut self) {
        self.is_closed = true;
        self.closed.notify_one();
    }
}

/// One scripted turn: the events a stream yields and what follows them,
/// or the error the stream open fails with.
type ScriptedTurn = Result<(Vec<AssistantMessageEvent>, Then), String>;

/// One recorded request: its context, credential, headers, and session id.
type RecordedRequest = (
    LlmContext,
    Option<String>,
    Option<BTreeMap<String, String>>,
    Option<String>,
);

/// One provider: records each request's context and options, signals when
/// a request is in flight, and serves its scripted turns in order (an
/// exhausted script fails the open).
#[derive(Default)]
struct Provider {
    turns: Mutex<VecDeque<ScriptedTurn>>,
    requests: Mutex<Vec<RecordedRequest>>,
    started: Notify,
}

impl Provider {
    fn with(turns: Vec<ScriptedTurn>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into()),
            ..Self::default()
        })
    }

    fn stream_fn(self: &Arc<Self>) -> StreamFn {
        let provider = Arc::clone(self);
        Arc::new(move |_model, context, options| {
            let provider = Arc::clone(&provider);
            Box::pin(async move {
                provider.requests.lock().unwrap().push((
                    context,
                    options.api_key.clone(),
                    options.headers.clone(),
                    options.session_id.clone(),
                ));
                provider.started.notify_one();
                let (events, then) = provider
                    .turns
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Err("no scripted turn".to_string()))
                    .map_err(|error| anyhow::anyhow!(error))?;
                Ok(Box::new(TestStream {
                    events: events.into(),
                    then,
                    closed: Arc::new(Notify::new()),
                    is_closed: false,
                    result: None,
                }) as Box<dyn ModelStream>)
            })
        })
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn pending_turns(&self) -> usize {
        self.turns.lock().unwrap().len()
    }
}

fn turn(provider: &Arc<Provider>, cancel: CancellationToken) -> Turn {
    Turn {
        prompt: "one turn".to_string(),
        model: model(),
        stream_fn: provider.stream_fn(),
        api_key: Some("test-key".to_string()),
        headers: Some(BTreeMap::from([(
            "Authorization".to_string(),
            "Bearer test-key".to_string(),
        )])),
        cancel,
        drain_timeout: Duration::from_secs(5),
        max_result_utf8_bytes: 1024,
    }
}

fn measured_zero() -> Usage {
    Usage::zero(Finality::Final, ZeroCost::Measured)
}

#[tokio::test]
async fn a_completed_turn_reports_joined_text_digest_and_provider_usage() {
    let provider = Provider::with(vec![Ok((
        vec![done(message(
            vec![text("hé"), text("llo")],
            StopReason::Stop,
            usage(99, 42.0),
        ))],
        Then::End,
    ))]);
    let report = run_turn(turn(&provider, CancellationToken::new())).await;
    assert_eq!(
        report,
        TurnReport {
            terminal: Terminal::Completed(ResultText::new("héllo".to_string())),
            usage: Usage {
                input_tokens: 2,
                output_tokens: 3,
                cache_read_tokens: 4,
                cache_write_tokens: 5,
                total_tokens: 99,
                cost_input: Some(1.0),
                cost_output: Some(2.0),
                cost_cache_read: Some(3.0),
                cost_cache_write: Some(4.0),
                cost_total: Some(42.0),
                ..measured_zero()
            },
            turns_started: 1,
        }
    );
    // Exactly one physical request: no system prompt, no tools, the prompt
    // as the only message, the request's own credential, no session id.
    let mut requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let (context, api_key, headers, session_id) = requests.remove(0);
    let mut context = serde_json::to_value(context).unwrap();
    // The request's own clock reading is the only non-scripted field.
    context["messages"][0]["timestamp"] = serde_json::json!(0);
    assert_eq!(
        (context, api_key, headers, session_id),
        (
            serde_json::json!({
                "system_prompt": null,
                "messages": [{ "role": "user", "content": "one turn", "timestamp": 0 }],
                "tools": []
            }),
            Some("test-key".to_string()),
            Some(BTreeMap::from([(
                "Authorization".to_string(),
                "Bearer test-key".to_string()
            )])),
            None,
        )
    );
}

#[tokio::test]
async fn a_provider_error_fails_without_a_retry() {
    let mut failed = message(Vec::new(), StopReason::Error, AgentUsage::zero());
    failed.error_message = Some("first request failed".to_string());
    let provider = Provider::with(vec![
        Ok((vec![done(failed)], Then::End)),
        Ok((
            vec![done(message(
                vec![text("fallback")],
                StopReason::Stop,
                AgentUsage::zero(),
            ))],
            Then::End,
        )),
    ]);
    let report = run_turn(turn(&provider, CancellationToken::new())).await;
    assert_eq!(
        report,
        TurnReport {
            terminal: Terminal::failed(FailureReason::ProviderFailed, "first request failed"),
            usage: measured_zero(),
            turns_started: 1,
        }
    );
    assert_eq!((provider.request_count(), provider.pending_turns()), (1, 1));
}

#[tokio::test]
async fn a_stream_that_cannot_open_is_a_provider_failure() {
    let provider = Provider::with(vec![Err("connection refused".to_string())]);
    let report = run_turn(turn(&provider, CancellationToken::new())).await;
    assert_eq!(
        report,
        TurnReport {
            terminal: Terminal::failed(FailureReason::ProviderFailed, "connection refused"),
            usage: measured_zero(),
            turns_started: 1,
        }
    );
}

#[tokio::test]
async fn a_tool_call_is_rejected_and_starts_no_second_request() {
    let call = AssistantContent::ToolCall(ToolCall {
        id: "call-1".to_string(),
        name: "forbidden".to_string(),
        arguments: serde_json::json!({}),
        thought_signature: None,
    });
    let provider = Provider::with(vec![
        Ok((
            vec![done(message(
                vec![call],
                StopReason::ToolUse,
                AgentUsage::zero(),
            ))],
            Then::End,
        )),
        Ok((
            vec![done(message(
                vec![text("must not run")],
                StopReason::Stop,
                AgentUsage::zero(),
            ))],
            Then::End,
        )),
    ]);
    let report = run_turn(turn(&provider, CancellationToken::new())).await;
    assert_eq!(
        report,
        TurnReport {
            terminal: Terminal::failed(FailureReason::UnexpectedToolCall, "Unexpected tool call"),
            usage: measured_zero(),
            turns_started: 1,
        }
    );
    assert_eq!((provider.request_count(), provider.pending_turns()), (1, 1));
}

#[tokio::test]
async fn an_oversized_result_returns_no_partial_text() {
    let provider = Provider::with(vec![Ok((
        vec![done(message(
            vec![text("éé")],
            StopReason::Stop,
            AgentUsage::zero(),
        ))],
        Then::End,
    ))]);
    let report = run_turn(Turn {
        max_result_utf8_bytes: 3,
        ..turn(&provider, CancellationToken::new())
    })
    .await;
    assert_eq!(
        report.terminal,
        Terminal::failed(
            FailureReason::ResultTooLarge,
            "Result exceeds maxResultUtf8Bytes"
        )
    );
}

#[tokio::test]
async fn invalid_provider_usage_fails_the_turn() {
    let provider = Provider::with(vec![Ok((
        vec![done(message(
            vec![text("ok")],
            StopReason::Stop,
            usage(5, f64::NAN),
        ))],
        Then::End,
    ))]);
    let report = run_turn(turn(&provider, CancellationToken::new())).await;
    assert_eq!(
        report,
        TurnReport {
            terminal: Terminal::failed(FailureReason::UsageInvalid, "Invalid provider usage"),
            usage: measured_zero(),
            turns_started: 1,
        }
    );
}

#[tokio::test]
async fn two_terminal_messages_make_the_capture_ambiguous() {
    let provider = Provider::with(vec![Ok((
        vec![
            done(message(
                vec![text("one")],
                StopReason::Stop,
                AgentUsage::zero(),
            )),
            done(message(
                vec![text("two")],
                StopReason::Stop,
                AgentUsage::zero(),
            )),
        ],
        Then::End,
    ))]);
    let report = run_turn(turn(&provider, CancellationToken::new())).await;
    assert_eq!(
        report,
        TurnReport {
            terminal: Terminal::Unknown(UnknownReason::TerminalCaptureAmbiguous),
            usage: measured_zero().with_finality(Finality::KnownPrefix),
            turns_started: 1,
        }
    );
}

#[tokio::test]
async fn a_pre_dispatch_cancel_makes_no_provider_request() {
    let provider = Provider::with(vec![Ok((
        vec![done(message(
            vec![text("must not run")],
            StopReason::Stop,
            AgentUsage::zero(),
        ))],
        Then::End,
    ))]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let report = run_turn(turn(&provider, cancel)).await;
    assert_eq!(
        report,
        TurnReport {
            terminal: Terminal::Cancelled,
            usage: measured_zero(),
            turns_started: 0,
        }
    );
    assert_eq!((provider.request_count(), provider.pending_turns()), (0, 1));
}

#[tokio::test]
async fn a_cancel_in_flight_closes_the_stream_and_settles_cancelled() {
    let provider = Provider::with(vec![Ok((Vec::new(), Then::AbortOnClose))]);
    let cancel = CancellationToken::new();
    let started = provider.started.notified();
    let pending = tokio::spawn(run_turn(turn(&provider, cancel.clone())));
    started.await;
    cancel.cancel();
    assert_eq!(
        pending.await.unwrap(),
        TurnReport {
            terminal: Terminal::Cancelled,
            usage: measured_zero(),
            turns_started: 1,
        }
    );
}

#[tokio::test]
async fn a_stream_that_never_drains_after_cancel_is_execution_unknown() {
    let provider = Provider::with(vec![Ok((Vec::new(), Then::Stall))]);
    let cancel = CancellationToken::new();
    let started = provider.started.notified();
    let pending = tokio::spawn(run_turn(Turn {
        drain_timeout: Duration::from_millis(1),
        ..turn(&provider, cancel.clone())
    }));
    started.await;
    cancel.cancel();
    assert_eq!(
        pending.await.unwrap(),
        TurnReport {
            terminal: Terminal::Unknown(UnknownReason::DrainTimeout),
            usage: measured_zero().with_finality(Finality::KnownPrefix),
            turns_started: 1,
        }
    );
}

#[tokio::test]
async fn a_cancel_after_the_terminal_keeps_the_completion() {
    let provider = Provider::with(vec![Ok((
        vec![done(message(
            vec![text("done")],
            StopReason::Stop,
            AgentUsage::zero(),
        ))],
        Then::End,
    ))]);
    let cancel = CancellationToken::new();
    let report = run_turn(turn(&provider, cancel.clone())).await;
    cancel.cancel();
    assert_eq!(
        report.terminal,
        Terminal::Completed(ResultText::new("done".to_string()))
    );
}
