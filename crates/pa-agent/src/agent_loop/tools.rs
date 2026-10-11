//! Tool-call batch execution: sequential and parallel dispatch, per-tool
//! preparation outcomes, batch termination, and tool-result message
//! emission.

use std::sync::Arc;

use super::run::clone_context;
use super::tool_call::{
    create_tool_result_message,
    emit_tool_execution_end,
    emit_tool_result_message,
    execute_prepared_tool_call,
    finalize_executed_tool_call,
    prepare_tool_call,
};
use super::{AgentEventSink, AgentLoopConfig};
use crate::abort::AbortSignal;
use crate::types::{
    AgentContext,
    AgentEvent,
    AgentTool,
    AgentToolResult,
    AssistantMessage,
    ToolCall,
    ToolExecutionMode,
    ToolResultContent,
    ToolResultMessage,
};

pub(crate) struct ExecutedToolCallBatch {
    pub(crate) messages: Vec<ToolResultMessage>,
    pub(crate) terminate: bool,
}

pub(crate) struct FinalizedToolCallOutcome {
    pub(crate) tool_call: ToolCall,
    pub(crate) result: AgentToolResult,
    pub(crate) is_error: bool,
}

pub(crate) async fn execute_tool_calls(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> anyhow::Result<ExecutedToolCallBatch> {
    let tool_calls = assistant_message
        .tool_calls()
        .into_iter()
        .cloned()
        .collect::<Vec<ToolCall>>();
    let has_sequential_tool_call = tool_calls.iter().any(|tc| {
        current_context
            .tools
            .iter()
            .find(|t| t.name() == tc.name)
            .and_then(|tool| tool.execution_mode())
            == Some(ToolExecutionMode::Sequential)
    });
    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential_tool_call {
        execute_tool_calls_sequential(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            signal,
            emit,
        )
        .await
    } else {
        execute_tool_calls_parallel(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            signal,
            emit,
        )
        .await
    }
}

async fn execute_tool_calls_sequential(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> anyhow::Result<ExecutedToolCallBatch> {
    let mut finalized_calls: Vec<FinalizedToolCallOutcome> = Vec::new();
    let mut messages: Vec<ToolResultMessage> = Vec::new();

    for tool_call in tool_calls {
        if signal.is_some_and(AbortSignal::is_aborted) {
            break;
        }

        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        })
        .await?;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            signal,
        )
        .await;
        let finalized = complete_tool_call(
            current_context,
            assistant_message,
            tool_call,
            preparation,
            config,
            signal,
            emit,
        )
        .await?;
        let tool_result_message = create_tool_result_message(&finalized);
        emit_tool_result_message(&tool_result_message, emit).await?;
        messages.push(tool_result_message);
        finalized_calls.push(finalized);

        if signal.is_some_and(AbortSignal::is_aborted) {
            break;
        }
    }

    Ok(ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    })
}

/// `tool_execution_end` is emitted in completion order (from inside the
/// concurrent tasks), while tool-result message events are emitted afterwards
/// in assistant source order, matching the TS reference.
async fn execute_tool_calls_parallel(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> anyhow::Result<ExecutedToolCallBatch> {
    enum TaskOrOutcome {
        Task(tokio::task::JoinHandle<anyhow::Result<FinalizedToolCallOutcome>>),
        // Boxed: the finalized outcome holds the tool call's ordered
        // argument map (insertion-ordered for wire parity), which dwarfs
        // the join handle and would trip `large_enum_variant`.
        Outcome(Box<FinalizedToolCallOutcome>),
    }

    let mut entries: Vec<TaskOrOutcome> = Vec::new();

    for tool_call in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        })
        .await?;

        let preparation = prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            signal,
        )
        .await;
        match preparation {
            Preparation::Immediate { .. } => {
                let finalized = complete_tool_call(
                    current_context,
                    assistant_message,
                    tool_call,
                    preparation,
                    config,
                    signal,
                    emit,
                )
                .await?;
                entries.push(TaskOrOutcome::Outcome(Box::new(finalized)));
            }
            Preparation::Prepared(prepared) => {
                let sink = Arc::clone(emit);
                let assistant_message = assistant_message.clone();
                let context = clone_context(current_context);
                let config = config.clone();
                let signal = signal.cloned().unwrap_or_default();
                let tool_call = tool_call.clone();
                let prepared = PreparedToolCall {
                    tool_call: prepared.tool_call.clone(),
                    tool: Arc::clone(&prepared.tool),
                    args: prepared.args.clone(),
                };
                // The task keeps the turn's span so its `tool.execute` nests under it.
                let handle = tokio::spawn(tracing::Instrument::in_current_span(async move {
                    complete_tool_call(
                        &context,
                        &assistant_message,
                        &tool_call,
                        Preparation::Prepared(prepared),
                        &config,
                        Some(&signal),
                        &sink,
                    )
                    .await
                }));
                entries.push(TaskOrOutcome::Task(handle));
            }
        }
    }

    // Promise.all semantics: await every entry; results stay in source order.
    let mut ordered_finalized_calls: Vec<FinalizedToolCallOutcome> = Vec::new();
    for entry in entries {
        match entry {
            TaskOrOutcome::Outcome(finalized) => ordered_finalized_calls.push(*finalized),
            TaskOrOutcome::Task(handle) => {
                let finalized = handle.await.map_err(|error| {
                    anyhow::anyhow!("Parallel tool execution task failed: {error}")
                })??;
                ordered_finalized_calls.push(finalized);
            }
        }
    }

    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for finalized in &ordered_finalized_calls {
        let tool_result_message = create_tool_result_message(finalized);
        emit_tool_result_message(&tool_result_message, emit).await?;
        messages.push(tool_result_message);
    }

    Ok(ExecutedToolCallBatch {
        messages,
        terminate: should_terminate_tool_batch(&ordered_finalized_calls),
    })
}

/// Execute a prepared tool call (or surface an immediate outcome such as an
/// unknown or blocked tool) and emit `tool_execution_end`, all inside a
/// `tool.execute` span. Preparation stays outside the span on both the
/// sequential and the parallel path, so the span always means the same thing.
/// An error result marks the span failed with the result's text; an abort is
/// reported as `tool.aborted` since it is not a failure of the tool.
#[tracing::instrument(
    level = "info",
    name = "tool.execute",
    skip_all,
    fields(
        tool.name = tool_call.name.as_str(),
        tool.call_id = tool_call.id.as_str(),
        tool.aborted = tracing::field::Empty,
        error = tracing::field::Empty,
    )
)]
async fn complete_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    preparation: Preparation,
    config: &AgentLoopConfig,
    signal: Option<&AbortSignal>,
    emit: &AgentEventSink,
) -> anyhow::Result<FinalizedToolCallOutcome> {
    let finalized = match preparation {
        Preparation::Immediate { result, is_error } => FinalizedToolCallOutcome {
            tool_call: tool_call.clone(),
            result,
            is_error,
        },
        Preparation::Prepared(prepared) => {
            let executed = execute_prepared_tool_call(&prepared, signal, emit).await;
            finalize_executed_tool_call(
                current_context,
                assistant_message,
                &prepared,
                executed,
                config,
                signal,
            )
            .await?
        }
    };
    if finalized.is_error {
        let span = tracing::Span::current();
        if signal.is_some_and(AbortSignal::is_aborted) {
            span.record("tool.aborted", true);
        } else {
            let text = finalized.result.content.iter().find_map(|part| match part {
                ToolResultContent::Text(text) if !text.text.is_empty() => Some(text.text.as_str()),
                ToolResultContent::Text(_) | ToolResultContent::Image(_) => None,
            });
            span.record("error", text.unwrap_or("tool execution failed"));
        }
    }
    emit_tool_execution_end(&finalized, emit).await?;
    Ok(finalized)
}

pub(crate) enum Preparation {
    Prepared(PreparedToolCall),
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
}

pub(crate) struct PreparedToolCall {
    pub(crate) tool_call: ToolCall,
    pub(crate) tool: Arc<dyn AgentTool>,
    pub(crate) args: serde_json::Value,
}

fn should_terminate_tool_batch(finalized_calls: &[FinalizedToolCallOutcome]) -> bool {
    !finalized_calls.is_empty()
        && finalized_calls
            .iter()
            .all(|finalized| finalized.result.terminate == Some(true))
}
