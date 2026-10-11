//! The one-turn inference boundary (TS `runWorkflowAgent`): exactly one
//! provider request with no tools and no system prompt, no retry, no
//! session, no persistence. Cancellation closes the provider stream; the
//! turn then has `drainTimeoutMs` to settle before the host reports that it
//! cannot know how the turn ended.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pa_agent::stream::{LlmContext, StreamFn, StreamRequestOptions};
use pa_agent::types::{
    AssistantContent,
    AssistantMessage,
    Message,
    Model,
    StopReason,
    UserContent,
    UserMessage,
};
use tokio_util::sync::CancellationToken;

use super::wire::{
    FailureReason,
    Finality,
    MAX_SAFE_INTEGER,
    ResultText,
    Terminal,
    UnknownReason,
    Usage,
    ZeroCost,
};

const MAX_COST: f64 = 1e15;

/// One turn's inputs: the provider transport already bound to the resolved
/// model and its credential.
pub(crate) struct Turn {
    pub prompt: String,
    pub model: Model,
    pub stream_fn: StreamFn,
    pub api_key: Option<String>,
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    pub cancel: CancellationToken,
    pub drain_timeout: Duration,
    pub max_result_utf8_bytes: u64,
}

/// How one turn settled, with the usage the host observed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnReport {
    pub terminal: Terminal,
    pub usage: Usage,
    pub turns_started: u8,
}

/// Which side won the race between the provider's terminal message and the
/// caller's cancellation (first wins, like the TS `winner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Winner {
    Terminal,
    Abort,
}

#[derive(Default)]
struct Observed {
    winner: Option<Winner>,
    terminal: Option<AssistantMessage>,
    terminals_seen: u32,
    unexpected_tool_call: bool,
    open_failure: Option<String>,
}

impl Observed {
    fn claim(&mut self, winner: Winner) {
        self.winner.get_or_insert(winner);
    }

    fn record_terminal(&mut self, message: AssistantMessage) {
        self.claim(Winner::Terminal);
        self.terminals_seen += 1;
        self.unexpected_tool_call |= has_tool_call(&message);
        self.terminal = Some(message);
    }
}

type Shared = Arc<Mutex<Observed>>;

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Observed> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

fn has_tool_call(message: &AssistantMessage) -> bool {
    message
        .content
        .iter()
        .any(|part| matches!(part, AssistantContent::ToolCall(_)))
}

/// Run the one provider turn and classify how it settled.
pub(crate) async fn run_turn(turn: Turn) -> TurnReport {
    if turn.cancel.is_cancelled() {
        // A pre-dispatch cancel: no provider I/O at all.
        return TurnReport {
            terminal: Terminal::Cancelled,
            usage: Usage::zero(Finality::Final, ZeroCost::Measured),
            turns_started: 0,
        };
    }
    let shared: Shared = Arc::default();
    let cancel = turn.cancel.clone();
    let mut request = tokio::spawn(provider_request(
        Arc::clone(&shared),
        turn.stream_fn,
        turn.model,
        turn.prompt,
        RequestAuth {
            api_key: turn.api_key,
            headers: turn.headers,
        },
        turn.cancel,
    ));
    let drained = tokio::select! {
        _ = &mut request => true,
        () = cancel.cancelled() => {
            lock(&shared).claim(Winner::Abort);
            tokio::time::timeout(turn.drain_timeout, &mut request).await.is_ok()
        }
    };
    if !drained {
        request.abort();
    }
    let report = classify(&lock(&shared), drained, turn.max_result_utf8_bytes);
    report
}

struct RequestAuth {
    api_key: Option<String>,
    headers: Option<std::collections::BTreeMap<String, String>>,
}

/// The physical request: open the stream, observe every terminal event,
/// close the stream when the caller cancels, and settle on its result.
async fn provider_request(
    shared: Shared,
    stream_fn: StreamFn,
    model: Model,
    prompt: String,
    auth: RequestAuth,
    cancel: CancellationToken,
) {
    let context = LlmContext {
        system_prompt: None,
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text(prompt),
            timestamp: now_ms(),
        })],
        tools: Vec::new(),
    };
    // Only the resolved credential and auth headers travel: no session id,
    // no payload/response hooks, no parent request headers.
    let options = StreamRequestOptions {
        api_key: auth.api_key,
        headers: auth.headers,
        ..StreamRequestOptions::default()
    };
    let mut stream = match stream_fn(model, context, options).await {
        Ok(stream) => stream,
        Err(error) => {
            lock(&shared).open_failure = Some(format!("{error:#}"));
            return;
        }
    };
    let mut closed = false;
    loop {
        let event = tokio::select! {
            event = stream.next_event() => event,
            () = cancel.cancelled(), if !closed => {
                stream.close();
                closed = true;
                continue;
            }
        };
        let Some(event) = event else { break };
        if let Some(message) = event.terminal_message() {
            lock(&shared).record_terminal(message.clone());
        }
    }
    if let Ok(message) = stream.result().await {
        let mut observed = lock(&shared);
        if observed.terminal.is_none() {
            observed.record_terminal(message);
        }
    }
}

/// The TS classification order: drain, usage, ambiguity, cancellation,
/// missing terminal, tool call, provider error, size, completion.
fn classify(observed: &Observed, drained: bool, max_result_utf8_bytes: u64) -> TurnReport {
    let turns_started = 1;
    let terminal_usage = observed.terminal.as_ref().map(|message| &message.usage);
    if !drained {
        return TurnReport {
            terminal: Terminal::Unknown(UnknownReason::DrainTimeout),
            usage: terminal_usage
                .and_then(capture_usage)
                .unwrap_or_else(|| Usage::zero(Finality::Final, ZeroCost::Measured))
                .with_finality(Finality::KnownPrefix),
            turns_started,
        };
    }
    let Some(usage) = terminal_usage.map_or_else(
        || Some(Usage::zero(Finality::Final, ZeroCost::Measured)),
        capture_usage,
    ) else {
        return TurnReport {
            terminal: Terminal::failed(FailureReason::UsageInvalid, "Invalid provider usage"),
            usage: Usage::zero(Finality::Final, ZeroCost::Measured),
            turns_started,
        };
    };
    let report = |terminal| TurnReport {
        terminal,
        usage,
        turns_started,
    };
    if observed.terminals_seen > 1 {
        return TurnReport {
            usage: usage.with_finality(Finality::KnownPrefix),
            ..report(Terminal::Unknown(UnknownReason::TerminalCaptureAmbiguous))
        };
    }
    let terminal = observed.terminal.as_ref();
    if observed.winner == Some(Winner::Abort)
        || terminal.is_some_and(|message| message.stop_reason == StopReason::Aborted)
    {
        return report(Terminal::Cancelled);
    }
    if let Some(error) = &observed.open_failure {
        return report(Terminal::failed(FailureReason::ProviderFailed, error));
    }
    let Some(terminal) = terminal else {
        return report(Terminal::failed(
            FailureReason::ResultMissing,
            "Authoritative terminal result missing",
        ));
    };
    if observed.unexpected_tool_call {
        return report(Terminal::failed(
            FailureReason::UnexpectedToolCall,
            "Unexpected tool call",
        ));
    }
    if terminal.stop_reason == StopReason::Error {
        return report(Terminal::failed(
            FailureReason::ProviderFailed,
            terminal
                .error_message
                .as_deref()
                .unwrap_or("Provider failed"),
        ));
    }
    let text: String = terminal
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => None,
        })
        .collect();
    if text.len() as u64 > max_result_utf8_bytes {
        return report(Terminal::failed(
            FailureReason::ResultTooLarge,
            "Result exceeds maxResultUtf8Bytes",
        ));
    }
    report(Terminal::Completed(ResultText::new(text)))
}

/// The provider's usage on the wire, or `None` when a count exceeds the
/// safe-integer range or a cost is not a finite value in `[0, 1e15]`.
fn capture_usage(usage: &pa_agent::types::Usage) -> Option<Usage> {
    let tokens = [
        usage.input,
        usage.output,
        usage.cache_read,
        usage.cache_write,
        usage.total_tokens,
    ];
    let cost = &usage.cost;
    let costs = [
        cost.input,
        cost.output,
        cost.cache_read,
        cost.cache_write,
        cost.total,
    ];
    if tokens.iter().any(|count| *count > MAX_SAFE_INTEGER)
        || costs
            .iter()
            .any(|cost| !cost.is_finite() || !(0.0..=MAX_COST).contains(cost))
    {
        return None;
    }
    Some(Usage {
        input_tokens: usage.input,
        output_tokens: usage.output,
        cache_read_tokens: usage.cache_read,
        cache_write_tokens: usage.cache_write,
        total_tokens: usage.total_tokens,
        cost_input: Some(cost.input),
        cost_output: Some(cost.output),
        cost_cache_read: Some(cost.cache_read),
        cost_cache_write: Some(cost.cache_write),
        cost_total: Some(cost.total),
        ..Usage::zero(Finality::Final, ZeroCost::Measured)
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests;
