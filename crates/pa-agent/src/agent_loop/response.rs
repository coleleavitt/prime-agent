//! Streaming one assistant response: context transform, LLM-bound message
//! conversion, the model stream event loop, the aborted-message finalize
//! path, and the empty-turn retry that wraps one or more attempts.

use std::sync::Arc;

use crate::abort::{is_abort_error, AbortSignal};
use crate::stream::{LlmContext, StreamFn, StreamRequestOptions, ToolDefinition};
use crate::types::{
    AgentContext, AgentEvent, AgentMessage, AssistantContent, AssistantMessage, StopReason, Usage,
};

use super::abort::{create_aborted_assistant_message, race_with_abort};
use super::{AgentEventSink, AgentLoopConfig};

/// Attempts per turn before an empty final turn becomes an error (upstream
/// #1896).
const MAX_EMPTY_TURN_ATTEMPTS: usize = 3;

/// A failure that interrupted empty-turn retries: the discarded attempts'
/// paid spend rides with the original error so the run-failure message can
/// still account for it. Displays as the original error.
#[derive(Debug)]
pub(crate) struct EmptyTurnRetryFailure {
    pub(crate) cause: anyhow::Error,
    pub(crate) discarded_usage: Vec<Usage>,
}

impl std::fmt::Display for EmptyTurnRetryFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:#}", self.cause)
    }
}

impl std::error::Error for EmptyTurnRetryFailure {}

/// No tool call and no visible text on a normal stop: completing here would
/// silently abandon the task. Error, aborted and length stops are signals of
/// their own, and thinking does not count as output.
fn is_empty_assistant_turn(message: &AssistantMessage) -> bool {
    match message.stop_reason {
        StopReason::Error | StopReason::Aborted | StopReason::Length => false,
        StopReason::Stop | StopReason::ToolUse => !message.content.iter().any(|part| match part {
            AssistantContent::ToolCall(_) => true,
            AssistantContent::Text(text) => !text.text.trim().is_empty(),
            AssistantContent::Thinking(_) => false,
        }),
    }
}

/// The silent-overflow shape (a normal stop whose input already exceeds the
/// window): empty by definition, and it must reach `message_end` untouched so
/// compaction recovery sees it. Mirrors the silent arm of
/// `pa_ai::is_context_overflow`, the only arm an empty normal stop can hit.
fn is_silent_context_overflow(message: &AssistantMessage, context_window: u64) -> bool {
    context_window > 0
        && message.stop_reason == StopReason::Stop
        && message.usage.input + message.usage.cache_read > context_window
}

/// Stream the assistant's response, silently re-requesting an empty final
/// turn up to [`MAX_EMPTY_TURN_ATTEMPTS`] times. A discarded attempt is
/// popped from the context before `message_end` (the durability edge), so it
/// is never resent nor persisted; its paid usage rides the surviving
/// message's `discarded_usage`. The last empty attempt settles as an error.
pub(crate) async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: Option<&StreamFn>,
) -> anyhow::Result<AssistantMessage> {
    let mut discarded_usage: Vec<Usage> = Vec::new();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut message =
            match stream_assistant_attempt(context, config, signal, emit, stream_fn).await {
                Ok(message) => message,
                Err(cause) if discarded_usage.is_empty() => return Err(cause),
                Err(cause) => {
                    return Err(anyhow::Error::new(EmptyTurnRetryFailure {
                        cause,
                        discarded_usage,
                    }))
                }
            };
        if is_empty_assistant_turn(&message)
            && !is_silent_context_overflow(&message, config.model.context_window)
        {
            if attempt < MAX_EMPTY_TURN_ATTEMPTS {
                tracing::warn!(
                    target: "pa_agent::empty_turn",
                    attempt,
                    "the model returned an empty response; retrying the request"
                );
                context.messages.pop();
                discarded_usage.push(message.usage);
                continue;
            }
            message.stop_reason = StopReason::Error;
            message.error_message = Some(format!(
                "Model returned an empty response (no output content or tool calls) {MAX_EMPTY_TURN_ATTEMPTS} times in a row"
            ));
            if let Some(last) = context.messages.last_mut() {
                *last = AgentMessage::from(message.clone());
            }
        }
        if !discarded_usage.is_empty() {
            message.discarded_usage = Some(std::mem::take(&mut discarded_usage));
            if let Some(last) = context.messages.last_mut() {
                *last = AgentMessage::from(message.clone());
            }
        }
        emit(AgentEvent::MessageEnd {
            message: AgentMessage::from(message.clone()),
        })
        .await?;
        return Ok(message);
    }
}

/// One provider call as an `llm.request` span (`llm.provider`, `llm.api`,
/// `llm.model`, `llm.base_url`); it ends when the response settles, with the
/// stop reason and token usage, and a provider error marks it failed.
#[tracing::instrument(
    level = "info",
    name = "llm.request",
    skip_all,
    fields(
        llm.provider = config.model.provider.as_str(),
        llm.api = config.model.api.as_str(),
        llm.model = config.model.id.as_str(),
        llm.base_url = config.model.base_url.as_str(),
        llm.stop_reason = tracing::field::Empty,
        llm.usage.input = tracing::field::Empty,
        llm.usage.output = tracing::field::Empty,
        error = tracing::field::Empty,
    )
)]
/// One attempt places its final message in the context and emits
/// `message_start`, never `message_end` (the caller decides whether the
/// attempt survives).
async fn stream_assistant_attempt(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: Option<&StreamFn>,
) -> anyhow::Result<AssistantMessage> {
    let mut partial_event: Option<Arc<crate::stream::AssistantMessageEvent>> = None;
    let mut added_partial = false;

    // The TS closure captures `partialMessage`/`addedPartial` by reference;
    // here the finish helper runs inline in the abort path below.
    macro_rules! finish_aborted_message {
        () => {{
            let final_message = create_aborted_assistant_message(
                config,
                partial_event.as_deref().and_then(event_partial),
            );
            if added_partial {
                *context.messages.last_mut().unwrap() = AgentMessage::from(final_message.clone());
            } else {
                context
                    .messages
                    .push(AgentMessage::from(final_message.clone()));
                emit(AgentEvent::MessageStart {
                    message: AgentMessage::from(final_message.clone()),
                })
                .await?;
            }
            final_message
        }};
    }

    let result = stream_assistant_response_inner(
        context,
        config,
        signal,
        emit,
        stream_fn,
        &mut partial_event,
        &mut added_partial,
    )
    .await;

    let span = tracing::Span::current();
    match result {
        Ok(message) => {
            let stop_reason = serde_json::to_value(message.stop_reason).ok();
            span.record(
                "llm.stop_reason",
                stop_reason.as_ref().and_then(serde_json::Value::as_str),
            )
            .record("llm.usage.input", message.usage.input)
            .record("llm.usage.output", message.usage.output);
            if message.stop_reason == crate::types::StopReason::Error {
                span.record(
                    "error",
                    message.error_message.as_deref().unwrap_or("provider error"),
                );
            }
            Ok(message)
        }
        Err(error) => {
            span.record("error", format!("{error:#}"));
            if signal.is_some_and(AbortSignal::is_aborted) && is_abort_error(&error) {
                return Ok(finish_aborted_message!());
            }
            Err(error)
        }
    }
}

/// Inner body of `streamAssistantResponse`.
// Direct port of the TS `try` block.
#[allow(clippy::too_many_lines)]
async fn stream_assistant_response_inner(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: Option<&StreamFn>,
    partial_event: &mut Option<Arc<crate::stream::AssistantMessageEvent>>,
    added_partial: &mut bool,
) -> anyhow::Result<AssistantMessage> {
    crate::abort::throw_if_aborted_signal(signal)?;

    let mut messages: Vec<AgentMessage> = context.messages.clone();
    if let Some(transform) = config.transform_context.as_ref() {
        messages = race_with_abort(
            transform(messages, signal.cloned().unwrap_or_default()),
            signal,
        )
        .await?;
    }

    let llm_messages = race_with_abort((config.convert_to_llm)(messages), signal).await?;

    let stream_fn = stream_fn.ok_or_else(|| {
        anyhow::anyhow!(
            "No stream function provided; the agent loop requires a model stream function (pa-ai integration supplies the default)"
        )
    })?;

    let resolved_api_key = match config.get_api_key.as_ref() {
        Some(get_api_key) => {
            match race_with_abort(get_api_key(config.model.provider.clone()), signal).await? {
                Some(key) => Some(key),
                None => config.api_key.clone(),
            }
        }
        None => config.api_key.clone(),
    };

    let llm_context = LlmContext {
        system_prompt: Some(
            config
                .get_system_prompt
                .as_ref()
                .map_or_else(|| context.system_prompt.clone(), |hook| hook()),
        ),
        messages: llm_messages,
        tools: context
            .tools
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.parameters().clone(),
            })
            .collect(),
    };

    let options = StreamRequestOptions {
        temperature: config.temperature,
        max_tokens: config.max_tokens,
        reasoning: config.reasoning,
        session_id: config.session_id.clone(),
        service_tier: config.service_tier,
        api_key: resolved_api_key,
        signal: signal.cloned().unwrap_or_default(),
        // The TS loop config's `onPayload`/`onResponse` ride every stream
        // call; the Rust request-timing seam composes them per request at
        // the `StreamFn` boundary instead.
        on_payload: None,
        on_response: None,
        headers: None,
        tool_choice: config.tool_choice,
    };

    let mut response = race_with_abort(
        stream_fn(config.model.clone(), llm_context, options),
        signal,
    )
    .await?;
    let mut repetition_guard = config
        .repetition_guard
        .map(crate::repetition_guard::RepetitionGuard::new);

    loop {
        let next = match signal {
            Some(signal) => {
                // TS races the iterator with `closeIterator` as the abort
                // callback: cancel the stream when the user aborts mid-read.
                let result = crate::abort::race_with_abort(response.next_event(), signal).await;
                if result.is_err() {
                    response.close();
                }
                result?
            }
            None => response.next_event().await,
        };
        let Some(event) = next else {
            break;
        };

        match event {
            crate::stream::AssistantMessageEvent::Start { partial } => {
                let message = AgentMessage::from(partial.clone());
                *added_partial = true;
                context.messages.push(message.clone());
                *partial_event = Some(Arc::new(crate::stream::AssistantMessageEvent::Start {
                    partial,
                }));
                emit(AgentEvent::MessageStart { message }).await?;
            }
            event if event.is_delta() => {
                let event = Arc::new(event);
                if let Some(partial) = event_partial(&event) {
                    *partial_event = Some(Arc::clone(&event));
                    emit(AgentEvent::MessageUpdate {
                        message: Arc::new(AgentMessage::from(partial.clone())),
                        assistant_message_event: Arc::clone(&event),
                    })
                    .await?;
                }
                // A degenerate loop ends the stream here instead of at the
                // output cap: the response settles as an error naming the
                // guard, with the loop trimmed to its first repeats.
                let looping = match (repetition_guard.as_mut(), &*event) {
                    (
                        Some(guard),
                        crate::stream::AssistantMessageEvent::TextDelta {
                            content_index,
                            partial,
                            ..
                        }
                        | crate::stream::AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            partial,
                            ..
                        },
                    ) => guard
                        .observe(partial, *content_index)
                        .map(|found| (found, partial.clone())),
                    _ => None,
                };
                if let Some((found, mut final_message)) = looping {
                    response.close();
                    tracing::warn!(
                        target: "pa_agent::repetition_guard",
                        period_chars = found.period_chars,
                        repeats = found.repeats,
                        "stopped a degenerate looping generation"
                    );
                    crate::repetition_guard::trim_loop(&mut final_message, &found);
                    final_message.stop_reason = crate::types::StopReason::Error;
                    final_message.stop_reason_raw =
                        Some(crate::repetition_guard::REPETITION_STOP_REASON.to_string());
                    final_message.error_message = Some(found.describe());
                    if *added_partial {
                        *context.messages.last_mut().unwrap() =
                            AgentMessage::from(final_message.clone());
                    } else {
                        context
                            .messages
                            .push(AgentMessage::from(final_message.clone()));
                        emit(AgentEvent::MessageStart {
                            message: AgentMessage::from(final_message.clone()),
                        })
                        .await?;
                    }
                    return Ok(final_message);
                }
            }
            ref event if event.terminal_message().is_some() => {
                let mut final_message = event.terminal_message().unwrap().clone();
                match race_with_abort(response.result(), signal).await {
                    Ok(result_message) => final_message = result_message,
                    Err(error) => {
                        let aborted = signal.is_some_and(AbortSignal::is_aborted);
                        if !(aborted && is_abort_error(&error)) {
                            return Err(error);
                        }
                    }
                }
                if *added_partial {
                    *context.messages.last_mut().unwrap() =
                        AgentMessage::from(final_message.clone());
                } else {
                    context
                        .messages
                        .push(AgentMessage::from(final_message.clone()));
                }
                if !*added_partial {
                    emit(AgentEvent::MessageStart {
                        message: AgentMessage::from(final_message.clone()),
                    })
                    .await?;
                }
                return Ok(final_message);
            }
            _ => {}
        }
    }

    // Stream ended without a terminal event: resolve the final message (TS
    // awaits `response.result()` here too).
    let final_message = race_with_abort(response.result(), signal).await?;
    if *added_partial {
        *context.messages.last_mut().unwrap() = AgentMessage::from(final_message.clone());
    } else {
        context
            .messages
            .push(AgentMessage::from(final_message.clone()));
        emit(AgentEvent::MessageStart {
            message: AgentMessage::from(final_message.clone()),
        })
        .await?;
    }
    Ok(final_message)
}

fn event_partial(event: &crate::stream::AssistantMessageEvent) -> Option<&AssistantMessage> {
    match event {
        crate::stream::AssistantMessageEvent::Start { partial }
        | crate::stream::AssistantMessageEvent::TextStart { partial, .. }
        | crate::stream::AssistantMessageEvent::TextDelta { partial, .. }
        | crate::stream::AssistantMessageEvent::TextEnd { partial, .. }
        | crate::stream::AssistantMessageEvent::ThinkingStart { partial, .. }
        | crate::stream::AssistantMessageEvent::ThinkingDelta { partial, .. }
        | crate::stream::AssistantMessageEvent::ThinkingEnd { partial, .. }
        | crate::stream::AssistantMessageEvent::ToolCallStart { partial, .. }
        | crate::stream::AssistantMessageEvent::ToolCallDelta { partial, .. }
        | crate::stream::AssistantMessageEvent::ToolCallEnd { partial, .. } => Some(partial),
        _ => None,
    }
}
