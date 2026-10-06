//! The agent turn loop: assistant turns, tool-call batches,
//! steering/follow-up/continuation message polling, and stop-hook
//! evaluation.

use crate::abort::AbortSignal;
use crate::stream::StreamFn;
use crate::types::{
    AgentContext, AgentEvent, AgentMessage, AssistantMessage, ShouldStopAfterTurnContext,
    StopReason, ToolCall, ToolResultMessage,
};

use super::abort::{
    poll_messages_unless_aborted, race_with_abort, settle_post_turn, PostTurnResult,
};
use super::response::stream_assistant_response;
use super::tools::execute_tool_calls;
use super::{AgentEventSink, AgentLoopConfig};

// Direct port of the TS turn loop.
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_loop(
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: Option<&StreamFn>,
) -> anyhow::Result<()> {
    let mut first_turn = true;
    let mut turn_index: u64 = 0;
    let mut last_turn: Option<ShouldStopAfterTurnContext> = None;
    // Consecutive output-limit auto-continuations (reset by any turn that
    // ends some other way).
    let mut length_continuations: u32 = 0;
    // The dropped-tool-call recovery runs at most once per run; its turn
    // carries a one-turn tool-choice override.
    let mut tool_intent_recovery_used = false;
    let mut recovery_tool_choice: Option<pa_types::ai::RequestToolChoice> = None;
    let mut pending_messages =
        poll_messages_unless_aborted(config.get_steering_messages.as_ref(), signal).await?;

    macro_rules! should_stop_before_turn {
        () => {
            !first_turn
                && config
                    .should_stop_before_turn
                    .as_ref()
                    .map(|hook| hook())
                    .unwrap_or(false)
        };
    }

    loop {
        crate::abort::throw_if_aborted_signal(signal)?;
        let mut has_more_tool_calls = true;

        while has_more_tool_calls || !pending_messages.is_empty() {
            crate::abort::throw_if_aborted_signal(signal)?;
            let emit_turn_start = !first_turn;
            first_turn = false;
            let recovery_config;
            let turn_config = match recovery_tool_choice.take() {
                Some(choice) => {
                    recovery_config = AgentLoopConfig {
                        tool_choice: Some(choice),
                        ..config.clone()
                    };
                    &recovery_config
                }
                None => config,
            };
            let turn = run_turn(
                TurnInput {
                    index: turn_index,
                    emit_turn_start,
                    pending_messages: std::mem::take(&mut pending_messages),
                },
                current_context,
                new_messages,
                turn_config,
                signal,
                emit,
                stream_fn,
            )
            .await?;
            turn_index += 1;
            if turn.terminal {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }
            let TurnOutcome {
                message,
                tool_results,
                has_more_tool_calls: more_tool_calls,
                ..
            } = turn;
            has_more_tool_calls = more_tool_calls;

            if signal.is_some_and(AbortSignal::is_aborted) {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }
            last_turn = Some(ShouldStopAfterTurnContext {
                message: message.clone(),
                tool_results: tool_results.clone(),
                context: clone_context(current_context),
                new_messages: new_messages.clone(),
            });

            let should_stop_result = settle_post_turn(
                race_with_abort(
                    async {
                        match config.should_stop_after_turn.as_ref() {
                            Some(hook) => hook(last_turn.clone().unwrap()).await,
                            None => Ok(false),
                        }
                    },
                    signal,
                ),
                signal,
            )
            .await?;
            match should_stop_result {
                PostTurnResult::Aborted | PostTurnResult::Completed(true) => {
                    emit(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await?;
                    return Ok(());
                }
                PostTurnResult::Completed(false) => {}
            }
            if should_stop_before_turn!() {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }

            let steering_messages_result = settle_post_turn(
                poll_messages_unless_aborted(config.get_steering_messages.as_ref(), signal),
                signal,
            )
            .await?;
            match steering_messages_result {
                PostTurnResult::Aborted => {
                    emit(AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    })
                    .await?;
                    return Ok(());
                }
                PostTurnResult::Completed(messages) => {
                    pending_messages = messages;
                    // Steering drained by this poll owns the turn boundary;
                    // stop only when it was empty.
                    if pending_messages.is_empty() && should_stop_before_turn!() {
                        emit(AgentEvent::AgentEnd {
                            messages: new_messages.clone(),
                        })
                        .await?;
                        return Ok(());
                    }
                    // A reply cut off at the output-token limit continues
                    // in the next turn (bounded); queued steering wins.
                    match &config.length_continuation {
                        Some(policy)
                            if pending_messages.is_empty()
                                && !has_more_tool_calls
                                && policy.continues(&message, length_continuations) =>
                        {
                            length_continuations += 1;
                            tracing::info!(
                                target: "pa_agent::length_continuation",
                                attempt = length_continuations,
                                max = policy.max_continuations,
                                "auto-continuing a reply cut off at the output-token limit"
                            );
                            pending_messages = vec![(policy.message)(
                                length_continuations,
                                policy.max_continuations,
                            )];
                        }
                        Some(_) | None => {
                            if message.stop_reason != StopReason::Length {
                                length_continuations = 0;
                            }
                        }
                    }
                }
            }
        }

        if should_stop_before_turn!() {
            break;
        }
        let follow_up_messages_result = settle_post_turn(
            poll_messages_unless_aborted(config.get_follow_up_messages.as_ref(), signal),
            signal,
        )
        .await?;
        let follow_up_messages = match follow_up_messages_result {
            PostTurnResult::Aborted => {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }
            PostTurnResult::Completed(messages) => messages,
        };
        if !follow_up_messages.is_empty() {
            pending_messages = follow_up_messages;
            continue;
        }

        if should_stop_before_turn!() {
            break;
        }
        let continuation_messages_result = match last_turn.clone() {
            Some(context) => {
                let continuation_op: crate::BoxFut<'static, anyhow::Result<Vec<AgentMessage>>> =
                    match config.get_continuation_messages.as_ref() {
                        Some(hook) => hook(context, signal.cloned().unwrap_or_default()),
                        None => Box::pin(async { Ok(Vec::new()) }),
                    };
                settle_post_turn(race_with_abort(continuation_op, signal), signal).await?
            }
            None => PostTurnResult::Completed(Vec::new()),
        };
        let continuation_messages = match continuation_messages_result {
            PostTurnResult::Aborted => {
                emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await?;
                return Ok(());
            }
            PostTurnResult::Completed(messages) => messages,
        };
        if !continuation_messages.is_empty() {
            pending_messages = continuation_messages;
            continue;
        }

        if should_stop_before_turn!() || signal.is_some_and(AbortSignal::is_aborted) {
            break;
        }
        // A reply that reported a tool call and delivered none (upstream
        // #2530) retries once, after every other continuation declined.
        if let (Some(hook), Some(context)) =
            (config.get_tool_intent_recovery.as_ref(), last_turn.clone())
        {
            if !tool_intent_recovery_used
                && config.model.api == "openai-completions"
                && matches!(
                    config.tool_choice,
                    None | Some(pa_types::ai::RequestToolChoice::Auto)
                )
                && super::ended_without_delivered_tool_call(&context.message, current_context)
            {
                let stop_reason = context.message.stop_reason;
                let recovery = settle_post_turn(
                    race_with_abort(
                        async {
                            // A failing hook declines (TS catches it).
                            Ok(hook(context).await.unwrap_or_else(|error| {
                                tracing::warn!(
                                    target: "pa_agent::tool_intent_recovery",
                                    error = %error,
                                    "the dropped-tool-call recovery hook failed; ending the run"
                                );
                                None
                            }))
                        },
                        signal,
                    ),
                    signal,
                )
                .await?;
                match recovery {
                    PostTurnResult::Aborted => {
                        emit(AgentEvent::AgentEnd {
                            messages: new_messages.clone(),
                        })
                        .await?;
                        return Ok(());
                    }
                    PostTurnResult::Completed(Some(message)) => {
                        tool_intent_recovery_used = true;
                        // A length finish may be ordinary truncation, so
                        // only a `toolUse` finish requires a tool call.
                        recovery_tool_choice = (stop_reason == StopReason::ToolUse)
                            .then_some(pa_types::ai::RequestToolChoice::Required);
                        tracing::info!(
                            target: "pa_agent::tool_intent_recovery",
                            stop_reason = ?stop_reason,
                            "retrying a reply that delivered no tool call"
                        );
                        pending_messages = vec![message];
                        continue;
                    }
                    PostTurnResult::Completed(None) => {}
                }
            }
        }

        break;
    }

    emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .await?;
    Ok(())
}

/// One turn's inputs: its index in this run, whether it opens with
/// `turn_start` (every turn but the run's first), and the steering or
/// follow-up messages it delivers before the assistant response.
struct TurnInput {
    index: u64,
    emit_turn_start: bool,
    pending_messages: Vec<AgentMessage>,
}

struct TurnOutcome {
    message: AssistantMessage,
    tool_results: Vec<ToolResultMessage>,
    /// The assistant stopped with an error or abort: the run ends without post-turn hooks.
    terminal: bool,
    has_more_tool_calls: bool,
}

/// At most this many distinct failed tool names ride the turn span.
const TURN_TOOL_ERROR_NAMES_LIMIT: usize = 8;

/// One assistant turn (`turn_start` ... `turn_end`) as an `agent.turn` span, so the
/// provider request and every tool execution nest under it. A failed response
/// marks the span failed; an abort is a normal outcome (`turn.aborted`). Tool
/// failures roll up as `turn.tool_errors` / `turn.tool_error_names` while the
/// turn itself stays ok.
#[tracing::instrument(
    level = "info",
    name = "agent.turn",
    skip_all,
    fields(
        session.id = config.session_id.as_deref(),
        turn.index = turn.index,
        llm.provider = config.model.provider.as_str(),
        llm.model = config.model.id.as_str(),
        turn.stop_reason = tracing::field::Empty,
        turn.aborted = tracing::field::Empty,
        turn.tool_calls = tracing::field::Empty,
        turn.tool_errors = tracing::field::Empty,
        turn.tool_error_names = tracing::field::Empty,
        error = tracing::field::Empty,
    )
)]
async fn run_turn(
    turn: TurnInput,
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
    stream_fn: Option<&StreamFn>,
) -> anyhow::Result<TurnOutcome> {
    let span = tracing::Span::current();
    if turn.emit_turn_start {
        emit(AgentEvent::TurnStart).await?;
    }

    for message in turn.pending_messages {
        emit(AgentEvent::MessageStart {
            message: message.clone(),
        })
        .await?;
        emit(AgentEvent::MessageEnd {
            message: message.clone(),
        })
        .await?;
        current_context.messages.push(message.clone());
        new_messages.push(message);
    }

    let message =
        stream_assistant_response(current_context, config, signal, emit, stream_fn).await?;
    new_messages.push(AgentMessage::from(message.clone()));
    let stop_reason = serde_json::to_value(message.stop_reason).ok();
    span.record(
        "turn.stop_reason",
        stop_reason.as_ref().and_then(serde_json::Value::as_str),
    );

    if message.stop_reason == StopReason::Error || message.stop_reason == StopReason::Aborted {
        if message.stop_reason == StopReason::Error {
            span.record(
                "error",
                message
                    .error_message
                    .as_deref()
                    .unwrap_or("assistant response failed"),
            );
        } else {
            span.record("turn.aborted", true);
        }
        emit(AgentEvent::TurnEnd {
            message: AgentMessage::from(message.clone()),
            tool_results: Vec::new(),
        })
        .await?;
        return Ok(TurnOutcome {
            message,
            tool_results: Vec::new(),
            terminal: true,
            has_more_tool_calls: false,
        });
    }

    let tool_calls = message
        .tool_calls()
        .into_iter()
        .cloned()
        .collect::<Vec<ToolCall>>();
    span.record("turn.tool_calls", tool_calls.len());

    let mut tool_results: Vec<ToolResultMessage> = Vec::new();
    let mut has_more_tool_calls = false;
    if !tool_calls.is_empty() {
        let executed_tool_batch =
            execute_tool_calls(current_context, &message, config, signal, emit).await?;
        tool_results.extend(executed_tool_batch.messages);
        has_more_tool_calls = !executed_tool_batch.terminate;

        for result in &tool_results {
            current_context
                .messages
                .push(AgentMessage::from(result.clone()));
            new_messages.push(AgentMessage::from(result.clone()));
        }
        let mut failed_names: Vec<&str> = Vec::new();
        let mut failed = 0usize;
        for result in tool_results.iter().filter(|result| result.is_error) {
            failed += 1;
            if !failed_names.contains(&result.tool_name.as_str()) {
                failed_names.push(&result.tool_name);
            }
        }
        span.record("turn.tool_errors", failed);
        if failed > 0 {
            failed_names.truncate(TURN_TOOL_ERROR_NAMES_LIMIT);
            span.record("turn.tool_error_names", failed_names.join(","));
        }
    }

    emit(AgentEvent::TurnEnd {
        message: AgentMessage::from(message.clone()),
        tool_results: tool_results.clone(),
    })
    .await?;
    Ok(TurnOutcome {
        message,
        tool_results,
        terminal: false,
        has_more_tool_calls,
    })
}

pub(crate) fn clone_context(context: &AgentContext) -> AgentContext {
    AgentContext {
        system_prompt: context.system_prompt.clone(),
        messages: context.messages.clone(),
        tools: context.tools.clone(),
    }
}
