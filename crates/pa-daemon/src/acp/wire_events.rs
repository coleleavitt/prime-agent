//! Daemon session-event mapping: the TS `acpUpdatesForSessionEvent` port
//! for the wire shapes a daemon worker streams (`message_start/update/end`,
//! `tool_execution_*`, `bash_*`, `compaction_end`, `goal_update`, ...): the
//! ACP frames the daemon worker's session events produce.
//!
//! Events with no ACP counterpart (`turn_end`, `auto_retry_*`,
//! `agent_begin/end`, `session_action_update`) map to nothing, exactly like
//! the TS switch's default arm.

use serde_json::{json, Value};

use super::meta::{prime_agent_meta, PrimeAgentCompactionMeta, PrimeAgentSessionMeta};
use super::types::{
    AcpSessionUpdate, AcpToolKind, AcpToolStatus, TextBlock, ToolCallContent, UserContentBlock,
};

/// The model-facing Python REPL tool.
const IPYTHON_TOOL_NAME: &str = "ipython";

/// Correlates streamed chunks with their owning assistant message (the
/// daemon stream carries the delta on `assistantMessageEvent`) and bash
/// output chunks with the run that produced them.
#[derive(Debug, Default)]
pub struct WireMappingState {
    next_assistant_message_sequence: u64,
    active_assistant_message_id: Option<String>,
    active_bash_run_id: Option<String>,
    /// The cell of each in-flight Python REPL call, by tool call id: the
    /// completion repeats it (a client replaces a call's content on update).
    ipython_cells: std::collections::HashMap<String, String>,
}

impl WireMappingState {
    fn start_assistant_message(&mut self) -> String {
        self.next_assistant_message_sequence += 1;
        let id = format!(
            "prime-agent-assistant-{}",
            self.next_assistant_message_sequence
        );
        self.active_assistant_message_id = Some(id.clone());
        id
    }

    fn message_started(&mut self) -> String {
        self.active_assistant_message_id
            .clone()
            .unwrap_or_else(|| self.start_assistant_message())
    }
}

/// The newest assistant stop reason carried by a `message_end` event (the
/// transport reads it after the turn for the stop-reason response).
pub struct AssistantStop {
    pub stop_reason: Option<String>,
}

/// Extract the assistant stop/error fields from one wire event, when the
/// event settles an assistant message.
pub fn assistant_stop(event: &Value) -> Option<AssistantStop> {
    if event.get("type").and_then(Value::as_str) != Some("message_end") {
        return None;
    }
    let message = event.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    Some(AssistantStop {
        stop_reason: message
            .get("stopReason")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Map one daemon session event to zero or more ACP updates.
pub fn wire_updates(event: &Value, state: &mut WireMappingState) -> Vec<AcpSessionUpdate> {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_type {
        "message_start" => {
            if event
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
                == Some("assistant")
            {
                state.start_assistant_message();
            }
            Vec::new()
        }
        "message_update" => {
            let message = event.get("message");
            if message
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
                != Some("assistant")
            {
                return Vec::new();
            }
            let stream = event.get("assistantMessageEvent");
            let delta = stream
                .and_then(|stream| stream.get("delta"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if delta.is_empty() {
                return Vec::new();
            }
            let message_id = state.message_started();
            match stream
                .and_then(|stream| stream.get("type"))
                .and_then(Value::as_str)
            {
                Some("thinking_delta") => vec![AcpSessionUpdate::AgentThoughtChunk {
                    message_id,
                    content: TextBlock::new(delta),
                }],
                Some("text_delta") => vec![AcpSessionUpdate::AgentMessageChunk {
                    message_id,
                    content: TextBlock::new(delta),
                }],
                _ => Vec::new(),
            }
        }
        "message_end" => {
            if event
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
                == Some("assistant")
            {
                state.active_assistant_message_id = None;
            }
            Vec::new()
        }
        "tool_execution_start" => {
            let tool_call_id = event
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let tool_name = event
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = event.get("args").cloned().unwrap_or(Value::Null);
            vec![tool_call_start(
                tool_call_id,
                tool_name,
                args,
                &mut state.ipython_cells,
            )]
        }
        // A cancelled or interrupted call never reports its end: the run's
        // end releases every cell still held.
        "agent_end" => {
            state.ipython_cells.clear();
            Vec::new()
        }
        "tool_execution_end" => {
            let tool_call_id = event
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let is_error = event
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let text = tool_result_text(event.get("result"));
            let rich = ipython_rich_output(event.get("result"));
            let cell = state.ipython_cells.remove(&tool_call_id);
            let update = AcpSessionUpdate::ToolCallUpdate {
                tool_call_id,
                status: Some(if is_error {
                    AcpToolStatus::Failed
                } else {
                    AcpToolStatus::Completed
                }),
                content: tool_call_end_content(cell.as_deref(), text),
                meta: rich.map(|rich| {
                    prime_agent_meta(&PrimeAgentSessionMeta {
                        ipython: Some(rich),
                        ..Default::default()
                    })
                }),
            };
            vec![update]
        }
        // User-level bash runs outside the tool-call lifecycle: a synthetic
        // tool call keyed by run id keeps the streamed chunks addressable.
        "bash_start" => {
            let command = event
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let run_id = event
                .get("runId")
                .and_then(Value::as_str)
                .map(str::to_string);
            state.active_bash_run_id.clone_from(&run_id);
            vec![AcpSessionUpdate::ToolCall {
                tool_call_id: bash_tool_call_id(run_id),
                title: command.clone(),
                kind: AcpToolKind::Execute,
                status: AcpToolStatus::InProgress,
                content: None,
                raw_input: json!({ "command": command }),
            }]
        }
        "bash_output" => {
            let chunk = event
                .get("chunk")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            vec![AcpSessionUpdate::ToolCallUpdate {
                tool_call_id: bash_tool_call_id(state.active_bash_run_id.clone()),
                status: Some(AcpToolStatus::InProgress),
                content: Some(vec![ToolCallContent::new(chunk)]),
                meta: None,
            }]
        }
        "bash_end" => {
            let run_id = event
                .get("runId")
                .and_then(Value::as_str)
                .map(str::to_string);
            if state.active_bash_run_id == run_id {
                state.active_bash_run_id = None;
            }
            let completed = event.get("exitCode").and_then(Value::as_i64) == Some(0)
                && !event
                    .get("cancelled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            vec![AcpSessionUpdate::ToolCallUpdate {
                tool_call_id: bash_tool_call_id(run_id),
                status: Some(if completed {
                    AcpToolStatus::Completed
                } else {
                    AcpToolStatus::Failed
                }),
                content: None,
                meta: None,
            }]
        }
        "goal_update" => {
            let goal = event.get("goal");
            vec![AcpSessionUpdate::SessionInfoUpdate {
                meta: prime_agent_meta(&PrimeAgentSessionMeta {
                    goal: Some(super::meta::PrimeAgentGoalMeta {
                        status: goal
                            .and_then(|goal| goal.get("status"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        objective: goal
                            .and_then(|goal| goal.get("objective"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        token_budget: goal
                            .and_then(|goal| goal.get("tokenBudget"))
                            .and_then(Value::as_u64),
                        tokens_used: goal
                            .and_then(|goal| goal.get("tokensUsed"))
                            .and_then(Value::as_u64),
                    }),
                    ..Default::default()
                }),
            }]
        }
        "compaction_end" => {
            let result = event.get("result");
            vec![AcpSessionUpdate::SessionInfoUpdate {
                meta: prime_agent_meta(&PrimeAgentSessionMeta {
                    compaction: Some(PrimeAgentCompactionMeta {
                        tokens_before: result
                            .and_then(|result| result.get("tokensBefore"))
                            .and_then(Value::as_u64),
                        summary: result
                            .and_then(|result| result.get("summary"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    }),
                    ..Default::default()
                }),
            }]
        }
        "rlm_child_update" => {
            let child = event.get("child");
            vec![AcpSessionUpdate::SessionInfoUpdate {
                meta: prime_agent_meta(&PrimeAgentSessionMeta {
                    subagents: Some(vec![super::meta::PrimeAgentSubagentMeta {
                        id: child
                            .and_then(|child| child.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        session_name: child
                            .and_then(|child| child.get("sessionName"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        status: child
                            .and_then(|child| child.get("status"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        model: child
                            .and_then(|child| child.get("model"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        depth: None,
                        token_count: child
                            .and_then(|child| child.get("tokenCount"))
                            .and_then(Value::as_u64),
                        error: child
                            .and_then(|child| child.get("error"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    }]),
                    ..Default::default()
                }),
            }]
        }
        "refine_complete" => {
            let result = event.get("result");
            let changes = result
                .and_then(|result| result.get("appliedEdits"))
                .and_then(Value::as_array)
                .map(|edits| {
                    edits
                        .iter()
                        .filter(|edit| edit.get("applied") == Some(&json!(true)))
                        .filter_map(|edit| {
                            let action = edit.get("action").and_then(Value::as_str)?;
                            let kind = edit.get("kind").and_then(Value::as_str)?;
                            let id = edit.get("id").and_then(Value::as_str)?;
                            Some(format!("{action} {kind}:{id}"))
                        })
                        .collect::<Vec<String>>()
                });
            vec![AcpSessionUpdate::SessionInfoUpdate {
                meta: prime_agent_meta(&PrimeAgentSessionMeta {
                    refinement: Some(super::meta::PrimeAgentRefinementMeta {
                        status: "complete".to_string(),
                        summary: result
                            .and_then(|result| result.get("summary"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        changes,
                        error: None,
                    }),
                    ..Default::default()
                }),
            }]
        }
        "auth_notice" => {
            let field = |name: &str| {
                event
                    .get(name)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            vec![AcpSessionUpdate::SessionInfoUpdate {
                meta: prime_agent_meta(&PrimeAgentSessionMeta {
                    auth_notice: Some(super::meta::PrimeAgentAuthNoticeMeta {
                        provider: field("provider"),
                        condition: field("condition"),
                        message: field("message"),
                    }),
                    ..Default::default()
                }),
            }]
        }
        "refine_failed" => {
            vec![AcpSessionUpdate::SessionInfoUpdate {
                meta: prime_agent_meta(&PrimeAgentSessionMeta {
                    refinement: Some(super::meta::PrimeAgentRefinementMeta {
                        status: "failed".to_string(),
                        summary: None,
                        changes: None,
                        error: event
                            .get("error")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    }),
                    ..Default::default()
                }),
            }]
        }
        "ipython_sent_agent_message" => {
            let message = event.get("message");
            vec![AcpSessionUpdate::SessionInfoUpdate {
                meta: prime_agent_meta(&PrimeAgentSessionMeta {
                    agent_message: Some(super::meta::PrimeAgentAgentMessageMeta {
                        tool_call_id: event
                            .get("toolCallId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        target: message
                            .and_then(|message| message.get("target"))
                            .and_then(|target| {
                                target
                                    .get("sessionName")
                                    .or_else(|| target.get("sessionId"))
                            })
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        delivery_status: message
                            .and_then(|message| message.get("deliveryStatus"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    }),
                    ..Default::default()
                }),
            }]
        }
        _ => Vec::new(),
    }
}

/// The context usage a settled assistant message reports (upstream #1351):
/// the message's own usage is the whole context at its end (nothing trails
/// it yet), against the session model's window. Nothing when the window is
/// unknown, or the message is aborted, errored, or carries no tokens — an
/// unknown reading is skipped, never reported as zero.
pub fn usage_update(event: &Value, context_window: u64) -> Option<AcpSessionUpdate> {
    if event.get("type").and_then(Value::as_str) != Some("message_end") || context_window == 0 {
        return None;
    }
    let usage = pa_types::usage::valid_assistant_usage(event.get("message")?)?;
    let used = pa_types::usage::calculate_context_tokens(&usage);
    (used > 0).then_some(AcpSessionUpdate::UsageUpdate {
        used,
        size: context_window,
    })
}

/// Replay a persisted transcript (the worker's `get_messages` rows) as the
/// ACP updates `session/load` streams before its response: user turns as
/// `user_message_chunk`, assistant text/thinking as message/thought chunks,
/// tool calls and their results as `tool_call` / `tool_call_update`, user
/// bash runs as a synthetic tool call, and a compaction summary as the
/// compaction meta. Rows ACP cannot represent (custom messages, branch
/// summaries) are skipped (upstream #2804's mapping).
pub fn transcript_updates(messages: &[Value]) -> Vec<AcpSessionUpdate> {
    let mut updates = Vec::new();
    let mut assistant_sequence = 0u64;
    let mut bash_sequence = 0u64;
    let mut cells = std::collections::HashMap::new();
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("user") => updates.extend(user_message_updates(message.get("content"))),
            Some("assistant") => {
                assistant_sequence += 1;
                let message_id = format!("prime-agent-replay-assistant-{assistant_sequence}");
                updates.extend(assistant_message_updates(message, &message_id, &mut cells));
            }
            Some("toolResult") => {
                let is_error = message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let tool_call_id = message
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let cell = cells.remove(&tool_call_id);
                updates.push(AcpSessionUpdate::ToolCallUpdate {
                    tool_call_id,
                    status: Some(if is_error {
                        AcpToolStatus::Failed
                    } else {
                        AcpToolStatus::Completed
                    }),
                    content: tool_call_end_content(
                        cell.as_deref(),
                        tool_result_text(Some(message)),
                    ),
                    meta: ipython_rich_output(Some(message)).map(|rich| {
                        prime_agent_meta(&PrimeAgentSessionMeta {
                            ipython: Some(rich),
                            ..Default::default()
                        })
                    }),
                });
            }
            Some("bashExecution") => {
                bash_sequence += 1;
                let tool_call_id = format!("prime-agent-replay-bash-{bash_sequence}");
                let command = message
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let output = message
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let completed = message.get("exitCode").and_then(Value::as_i64) == Some(0)
                    && !message
                        .get("cancelled")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                updates.push(AcpSessionUpdate::ToolCall {
                    tool_call_id: tool_call_id.clone(),
                    title: command.clone(),
                    kind: AcpToolKind::Execute,
                    status: AcpToolStatus::InProgress,
                    content: None,
                    raw_input: json!({ "command": command }),
                });
                updates.push(AcpSessionUpdate::ToolCallUpdate {
                    tool_call_id,
                    status: Some(if completed {
                        AcpToolStatus::Completed
                    } else {
                        AcpToolStatus::Failed
                    }),
                    content: (!output.is_empty()).then(|| vec![ToolCallContent::new(output)]),
                    meta: None,
                });
            }
            Some("compactionSummary") => {
                updates.push(AcpSessionUpdate::SessionInfoUpdate {
                    meta: prime_agent_meta(&PrimeAgentSessionMeta {
                        compaction: Some(PrimeAgentCompactionMeta {
                            tokens_before: message.get("tokensBefore").and_then(Value::as_u64),
                            summary: message
                                .get("summary")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        }),
                        ..Default::default()
                    }),
                });
            }
            _ => {}
        }
    }
    updates
}

/// A user message's content: a bare string or text/image blocks; empty
/// text drops out.
fn user_message_updates(content: Option<&Value>) -> Vec<AcpSessionUpdate> {
    let chunk = |content| AcpSessionUpdate::UserMessageChunk { content };
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![chunk(UserContentBlock::Text { text: text.clone() })]
        }
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| {
                        chunk(UserContentBlock::Text {
                            text: text.to_string(),
                        })
                    }),
                Some("image") => {
                    let data = block.get("data").and_then(Value::as_str)?;
                    let mime_type = block.get("mimeType").and_then(Value::as_str)?;
                    Some(chunk(UserContentBlock::Image {
                        data: data.to_string(),
                        mime_type: mime_type.to_string(),
                    }))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// An assistant message's blocks: text and thinking chunks under one
/// message id, tool calls as in-progress `tool_call`s (the result row
/// settles them).
fn assistant_message_updates(
    message: &Value,
    message_id: &str,
    cells: &mut std::collections::HashMap<String, String>,
) -> Vec<AcpSessionUpdate> {
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| {
            let text_of = |key: &str| {
                block
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(TextBlock::new)
            };
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    text_of("text").map(|content| AcpSessionUpdate::AgentMessageChunk {
                        message_id: message_id.to_string(),
                        content,
                    })
                }
                Some("thinking") => {
                    text_of("thinking").map(|content| AcpSessionUpdate::AgentThoughtChunk {
                        message_id: message_id.to_string(),
                        content,
                    })
                }
                Some("toolCall") => {
                    let tool_name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let arguments = block.get("arguments").cloned().unwrap_or(Value::Null);
                    Some(tool_call_start(
                        block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        tool_name,
                        arguments,
                        cells,
                    ))
                }
                _ => None,
            }
        })
        .collect()
}

/// The `tool_call` a started tool execution opens, live or replayed: the
/// Python REPL's cell travels as `rawInput.code`, titles the call, and rides
/// a fenced content block (held in `cells` for the completion to repeat).
fn tool_call_start(
    tool_call_id: String,
    tool_name: &str,
    args: Value,
    cells: &mut std::collections::HashMap<String, String>,
) -> AcpSessionUpdate {
    let cell = (tool_name == IPYTHON_TOOL_NAME)
        .then(|| args.get("code").and_then(Value::as_str).map(str::to_string))
        .flatten();
    let Some(code) = cell else {
        return AcpSessionUpdate::ToolCall {
            tool_call_id,
            title: tool_name.to_string(),
            kind: AcpToolKind::of_tool(tool_name),
            status: AcpToolStatus::InProgress,
            content: None,
            raw_input: args,
        };
    };
    cells.insert(tool_call_id.clone(), code.clone());
    AcpSessionUpdate::ToolCall {
        tool_call_id,
        title: ipython_cell_title(&code),
        kind: AcpToolKind::of_tool(tool_name),
        status: AcpToolStatus::InProgress,
        content: Some(vec![ipython_cell_content(&code)]),
        raw_input: json!({ "code": code }),
    }
}

/// A completed call's content: the held cell (a client replaces the call's
/// content on update, so the cell is repeated), then the result text.
fn tool_call_end_content(cell: Option<&str>, text: Option<String>) -> Option<Vec<ToolCallContent>> {
    let content: Vec<ToolCallContent> = cell
        .map(ipython_cell_content)
        .into_iter()
        .chain(text.map(ToolCallContent::new))
        .collect();
    (!content.is_empty()).then_some(content)
}

/// The longest title a cell gets, in characters.
const CELL_TITLE_MAX_CHARS: usize = 120;

/// Characters a one-line title never carries: controls (tab and newline
/// aside, which the line split handles) and bidi overrides, which could
/// reorder or blank the row that claims to describe the cell.
fn title_unsafe(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{8}'
        | '\u{b}'..='\u{1f}'
        | '\u{7f}'..='\u{9f}'
        | '\u{200e}'
        | '\u{200f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2066}'..='\u{2069}')
}

/// One-line label for a Python REPL cell (upstream #1309): its first
/// non-blank line, a leading cell magic paired with the next line (`%%bash`
/// alone names the interpreter, not the work), capped at
/// [`CELL_TITLE_MAX_CHARS`], with `· +N lines` for the rest. A blank cell
/// keeps the generic title.
fn ipython_cell_title(code: &str) -> String {
    let cleaned: String = code.chars().filter(|c| !title_unsafe(*c)).collect();
    let lines: Vec<&str> = cleaned
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .collect();
    let Some(first) = lines.first() else {
        return "Python cell".to_string();
    };
    let mut title = first.trim().to_string();
    if title.starts_with("%%") {
        if let Some(next) = lines.get(1) {
            title = format!("{title} \u{b7} {}", next.trim());
        }
    }
    if title.chars().count() > CELL_TITLE_MAX_CHARS {
        title = title.chars().take(CELL_TITLE_MAX_CHARS - 1).collect();
        title.push('\u{2026}');
    }
    match lines.len() - 1 {
        0 => title,
        remaining => format!("{title} \u{b7} +{remaining} lines"),
    }
}

/// The cell as a fenced `python` block whose fence outruns every backtick
/// run in the source, so the cell cannot close it and escape into Markdown.
fn ipython_cell_content(code: &str) -> ToolCallContent {
    let longest_run = code
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat((longest_run + 1).max(3));
    ToolCallContent::new(format!("{fence}python\n{code}\n{fence}"))
}

/// The synthetic tool-call id of a user-level bash run:
/// `prime-agent-bash-<runId>`; a run without an id keys the bare prefix
/// (TS `bashToolCallId`).
fn bash_tool_call_id(run_id: Option<String>) -> String {
    match run_id {
        Some(run_id) => format!("prime-agent-bash-{run_id}"),
        None => "prime-agent-bash".to_string(),
    }
}

/// TS `toolResultText`: the text of a tool result, wherever the engine
/// carries it. Empty text blocks drop out before the join and an empty
/// text yields `None`, so no update carries empty content (the TS call
/// site's `text ? { content } : {}`).
fn tool_result_text(result: Option<&Value>) -> Option<String> {
    let result = result?;
    if let Some(text) = result.as_str() {
        return (!text.is_empty()).then(|| text.to_string());
    }
    if let Some(output) = result.get("output").and_then(Value::as_str) {
        return (!output.is_empty()).then(|| output.to_string());
    }
    let content = result.get("content")?.as_array()?;
    let parts: Vec<String> = content
        .iter()
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .flatten()
        })
        .filter(|text| !text.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Decoded byte length of a base64 payload, without materializing it.
fn base64_byte_length(data: &str) -> u64 {
    let padding = if data.ends_with("==") {
        2
    } else {
        u64::from(data.ends_with('='))
    };
    (data.len() as u64 * 3 / 4).saturating_sub(padding)
}

/// Media and diffs ride the namespaced meta.
fn ipython_rich_output(result: Option<&Value>) -> Option<Value> {
    let details = result?.get("details")?;
    let attachments = details
        .get("attachments")
        .and_then(Value::as_array)
        .map(|attachments| {
            attachments
                .iter()
                .map(|attachment| {
                    let mut row = serde_json::Map::new();
                    if let Some(mime_type) = attachment.get("mimeType").and_then(Value::as_str) {
                        row.insert("mimeType".to_string(), json!(mime_type));
                    }
                    if let Some(path) = attachment.get("path").and_then(Value::as_str) {
                        row.insert("path".to_string(), json!(path));
                    }
                    if let Some(bytes) = attachment
                        .get("data")
                        .and_then(Value::as_str)
                        .map(base64_byte_length)
                    {
                        row.insert("bytes".to_string(), json!(bytes));
                    }
                    Value::Object(row)
                })
                .collect::<Vec<_>>()
        })
        .filter(|attachments| !attachments.is_empty());
    let diff_count = details
        .get("diffs")
        .and_then(Value::as_array)
        .map(|diffs| diffs.len() as u64);
    if attachments.is_none() && diff_count.is_none() {
        return None;
    }
    let mut meta = serde_json::Map::new();
    if let Some(attachments) = attachments {
        meta.insert("attachments".to_string(), Value::Array(attachments));
    }
    if let Some(diff_count) = diff_count {
        meta.insert("diffCount".to_string(), json!(diff_count));
    }
    Some(Value::Object(meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_deltas_map_to_chunks() {
        let mut state = WireMappingState::default();
        let updates = wire_updates(
            &json!({
                "type": "message_update",
                "message": { "role": "assistant" },
                "assistantMessageEvent": { "type": "thinking_delta", "delta": "think" },
            }),
            &mut state,
        );
        assert_eq!(updates.len(), 1);
        assert_eq!(
            serde_json::to_value(&updates[0]).unwrap()["sessionUpdate"],
            "agent_thought_chunk"
        );
        let updates = wire_updates(
            &json!({
                "type": "message_update",
                "message": { "role": "assistant" },
                "assistantMessageEvent": { "type": "text_delta", "delta": "answer" },
            }),
            &mut state,
        );
        assert_eq!(updates.len(), 1);
        assert_eq!(
            serde_json::to_value(&updates[0]).unwrap()["sessionUpdate"],
            "agent_message_chunk"
        );
    }

    #[test]
    fn goal_update_maps_to_the_namespaced_goal_meta() {
        // The GoalState fields the meta carries,
        // nothing else.
        let mut state = WireMappingState::default();
        let updates = wire_updates(
            &json!({
                "type": "goal_update",
                "goal": {
                    "active": true,
                    "status": "active",
                    "goalId": "g1",
                    "objective": "Name a river",
                    "tokenBudget": 500,
                    "tokensUsed": 0,
                    "timeUsedSeconds": 0,
                    "continuationsUsed": 0,
                },
            }),
            &mut state,
        );
        assert_eq!(updates.len(), 1);
        let value = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(value["sessionUpdate"], "session_info_update");
        assert_eq!(
            value["_meta"]["ai.primeintellect.prime-agent"]["goal"],
            json!({
                "status": "active",
                "objective": "Name a river",
                "tokenBudget": 500,
                "tokensUsed": 0,
            })
        );
    }

    #[test]
    fn user_messages_and_lifecycle_events_map_to_nothing() {
        let mut state = WireMappingState::default();
        for event_type in [
            "message_start",
            "message_end",
            "turn_end",
            "agent_begin",
            "session_action_update",
            "auto_retry_start",
        ] {
            let updates = wire_updates(
                &json!({ "type": event_type, "message": { "role": "user" } }),
                &mut state,
            );
            assert!(updates.is_empty(), "{event_type} maps to nothing");
        }
    }

    #[test]
    fn tool_calls_and_completions_map_like_the_ts_adapter() {
        let mut state = WireMappingState::default();
        let updates = wire_updates(
            &json!({
                "type": "tool_execution_start",
                "toolCallId": "call-1",
                "toolName": "ipython",
                "args": { "code": "print(1)" },
            }),
            &mut state,
        );
        assert_eq!(
            updates[0].to_bare_value(),
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "print(1)",
                "kind": "execute",
                "status": "in_progress",
                "content": [{ "type": "content", "content": { "type": "text", "text": "```python\nprint(1)\n```" } }],
                "rawInput": { "code": "print(1)" },
            })
        );
        let updates = wire_updates(
            &json!({
                "type": "tool_execution_end",
                "toolCallId": "call-1",
                "result": { "output": "1\n" },
                "isError": false,
            }),
            &mut state,
        );
        // The completion repeats the cell (a client replaces content on
        // update) and releases it.
        assert_eq!(
            updates[0].to_bare_value(),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
                "content": [
                    { "type": "content", "content": { "type": "text", "text": "```python\nprint(1)\n```" } },
                    { "type": "content", "content": { "type": "text", "text": "1\n" } },
                ],
            })
        );
        assert!(state.ipython_cells.is_empty());
    }

    fn cell_title(code: &str) -> String {
        let mut state = WireMappingState::default();
        let update = wire_updates(
            &json!({
                "type": "tool_execution_start",
                "toolCallId": "call-1",
                "toolName": "ipython",
                "args": { "code": code },
            }),
            &mut state,
        );
        update[0].to_bare_value()["title"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn ipython_calls_are_titled_by_their_cell() {
        assert_eq!(
            cell_title("import os\nos.getcwd()"),
            "import os \u{b7} +1 lines"
        );
        // A bare cell magic names the interpreter, not the work.
        assert_eq!(
            cell_title("%%bash\ngit status --short"),
            "%%bash \u{b7} git status --short \u{b7} +1 lines"
        );
        assert_eq!(cell_title("   \n\n"), "Python cell");
        assert_eq!(
            cell_title(&"x".repeat(200)),
            format!("{}\u{2026}", "x".repeat(119))
        );
        // Controls and bidi overrides never reach the one-line title.
        assert_eq!(cell_title("\u{202e}print(1)\u{7}"), "print(1)");
    }

    #[test]
    fn the_cell_fence_outruns_the_cells_own_backticks() {
        assert_eq!(
            ipython_cell_content("print(\"\"\"```\"\"\")").content.text,
            "````python\nprint(\"\"\"```\"\"\")\n````"
        );
        assert_eq!(
            ipython_cell_content("x = '`````'").content.text,
            "``````python\nx = '`````'\n``````"
        );
    }

    #[test]
    fn agent_end_releases_cells_whose_calls_never_ended() {
        let mut state = WireMappingState::default();
        wire_updates(
            &json!({
                "type": "tool_execution_start",
                "toolCallId": "call-1",
                "toolName": "ipython",
                "args": { "code": "while True: pass" },
            }),
            &mut state,
        );
        assert_eq!(state.ipython_cells.len(), 1);
        assert!(wire_updates(&json!({ "type": "agent_end" }), &mut state).is_empty());
        assert!(state.ipython_cells.is_empty());
    }

    #[test]
    fn compaction_end_publishes_the_meta_payload() {
        let mut state = WireMappingState::default();
        let updates = wire_updates(
            &json!({
                "type": "compaction_end",
                "result": { "tokensBefore": 4200, "summary": "a summary" },
            }),
            &mut state,
        );
        let value = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(value["sessionUpdate"], "session_info_update");
        assert_eq!(
            value["_meta"]["ai.primeintellect.prime-agent"]["compaction"],
            json!({ "tokensBefore": 4200, "summary": "a summary" })
        );
    }

    #[test]
    fn assistant_stop_reason_is_captured_from_message_end() {
        let stop = assistant_stop(&json!({
            "type": "message_end",
            "message": { "role": "assistant", "stopReason": "end_turn" },
        }))
        .expect("assistant message_end");
        assert_eq!(stop.stop_reason.as_deref(), Some("end_turn"));
        assert!(assistant_stop(&json!({
            "type": "message_end",
            "message": { "role": "user" },
        }))
        .is_none());
    }

    #[test]
    fn empty_tool_results_carry_no_content() {
        let mut state = WireMappingState::default();
        let updates = wire_updates(
            &json!({
                "type": "tool_execution_end",
                "toolCallId": "t1",
                "result": { "output": "" },
                "isError": false,
            }),
            &mut state,
        );
        let value = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(value["sessionUpdate"], "tool_call_update");
        assert_eq!(value["status"], "completed");
        assert!(value.get("content").is_none(), "no empty content: {value}");
    }

    #[test]
    fn empty_text_blocks_drop_out_of_the_joined_result() {
        let mut state = WireMappingState::default();
        let updates = wire_updates(
            &json!({
                "type": "tool_execution_end",
                "toolCallId": "t1",
                "result": { "content": [
                    { "type": "text", "text": "" },
                    { "type": "text", "text": "a" },
                ] },
                "isError": false,
            }),
            &mut state,
        );
        let value = serde_json::to_value(&updates[0]).unwrap();
        assert_eq!(value["content"][0]["content"]["text"], "a");
    }

    fn namespaced(update: &AcpSessionUpdate) -> Value {
        update.to_bare_value()["_meta"]["ai.primeintellect.prime-agent"].clone()
    }

    #[test]
    fn rlm_child_update_maps_to_the_subagents_meta() {
        let mut state = WireMappingState::default();
        let updates = wire_updates(
            &json!({
                "type": "rlm_child_update",
                "child": {
                    "id": "child-1",
                    "parentId": "node-1",
                    "activeSessionId": "child-live",
                    "sessionName": "worker-a",
                    "model": "z-ai/glm-5.3-flash",
                    "label": "run the lane task",
                    "status": "running",
                    "durationMs": 500,
                    "sessionDir": "/sessions/child-1",
                },
            }),
            &mut state,
        );
        let value = updates[0].to_bare_value();
        assert_eq!(value["sessionUpdate"], "session_info_update");
        assert_eq!(
            namespaced(&updates[0])["subagents"],
            json!([{
                "id": "child-1",
                "sessionName": "worker-a",
                "status": "running",
                "model": "z-ai/glm-5.3-flash",
            }])
        );
        let updates = wire_updates(
            &json!({
                "type": "rlm_child_update",
                "child": {
                    "id": "child-2",
                    "status": "cancelled",
                    "error": "Deleted by parent orchestrator",
                },
            }),
            &mut state,
        );
        assert_eq!(
            namespaced(&updates[0])["subagents"],
            json!([{
                "id": "child-2",
                "status": "cancelled",
                "error": "Deleted by parent orchestrator",
            }])
        );
    }

    #[test]
    fn refine_complete_maps_the_applied_edits_changes() {
        let mut state = WireMappingState::default();
        let updates = wire_updates(
            &json!({
                "type": "refine_complete",
                "result": {
                    "id": "ref-1",
                    "summary": "applied 2 edits",
                    "appliedEdits": [
                        { "action": "create", "kind": "memory", "id": "x", "applied": true },
                        { "action": "update", "kind": "skill", "id": "y", "applied": true },
                        { "action": "delete", "kind": "prompt", "id": "z", "applied": false },
                    ],
                },
            }),
            &mut state,
        );
        assert_eq!(
            namespaced(&updates[0])["refinement"],
            json!({
                "status": "complete",
                "summary": "applied 2 edits",
                "changes": ["create memory:x", "update skill:y"],
            })
        );
        let updates = wire_updates(
            &json!({
                "type": "refine_complete",
                "result": { "id": "ref-2", "summary": "no edits" },
            }),
            &mut state,
        );
        assert_eq!(
            namespaced(&updates[0])["refinement"],
            json!({ "status": "complete", "summary": "no edits" })
        );
    }

    #[test]
    fn refine_failed_maps_the_error_meta() {
        let mut state = WireMappingState::default();
        let updates = wire_updates(
            &json!({ "type": "refine_failed", "error": "Summarization failed: no responses" }),
            &mut state,
        );
        assert_eq!(
            namespaced(&updates[0])["refinement"],
            json!({ "status": "failed", "error": "Summarization failed: no responses" })
        );
    }

    #[test]
    fn auth_notice_maps_the_auth_notice_meta() {
        let mut state = WireMappingState::default();
        let updates = wire_updates(
            &json!({
                "type": "auth_notice",
                "provider": "anthropic",
                "condition": "revoked:main",
                "message": "Your Anthropic login main was revoked; using pool. Run /login anthropic to restore it.",
            }),
            &mut state,
        );
        assert_eq!(updates.len(), 1);
        assert_eq!(
            namespaced(&updates[0])["authNotice"],
            json!({
                "provider": "anthropic",
                "condition": "revoked:main",
                "message": "Your Anthropic login main was revoked; using pool. Run /login anthropic to restore it.",
            })
        );
    }

    #[test]
    fn ipython_sent_agent_message_maps_the_target_fallback() {
        let mut state = WireMappingState::default();
        let updates = wire_updates(
            &json!({
                "type": "ipython_sent_agent_message",
                "toolCallId": "t7",
                "message": {
                    "id": "agentmsg_1",
                    "message": "Ping.",
                    "deliveryStatus": "delivered",
                    "target": {
                        "activeSessionId": "peer-live",
                        "sessionId": "peer-session",
                        "sessionName": "Worker",
                    },
                },
            }),
            &mut state,
        );
        assert_eq!(
            namespaced(&updates[0])["agentMessage"],
            json!({
                "toolCallId": "t7",
                "target": "Worker",
                "deliveryStatus": "delivered",
            })
        );
        let updates = wire_updates(
            &json!({
                "type": "ipython_sent_agent_message",
                "toolCallId": "t8",
                "message": {
                    "id": "agentmsg_2",
                    "message": "Ping.",
                    "deliveryStatus": "queued",
                    "target": {
                        "activeSessionId": "peer-live",
                        "sessionId": "peer-session",
                    },
                },
            }),
            &mut state,
        );
        assert_eq!(
            namespaced(&updates[0])["agentMessage"],
            json!({
                "toolCallId": "t8",
                "target": "peer-session",
                "deliveryStatus": "queued",
            })
        );
    }

    #[test]
    fn transcript_replay_maps_every_representable_row() {
        let messages = json!([
            { "role": "user", "content": "Name a river." },
            { "role": "user", "content": [
                { "type": "text", "text": "" },
                { "type": "text", "text": "and this" },
                { "type": "image", "data": "AAAA", "mimeType": "image/png" },
            ] },
            { "role": "assistant", "content": [
                { "type": "thinking", "thinking": "rivers" },
                { "type": "text", "text": "The Nile." },
                { "type": "toolCall", "id": "call-1", "name": "ipython", "arguments": { "code": "6*7" } },
            ] },
            { "role": "toolResult", "toolCallId": "call-1", "toolName": "ipython",
              "content": [{ "type": "text", "text": "42" }], "isError": false },
            { "role": "bashExecution", "command": "false", "output": "", "exitCode": 1 },
            { "role": "custom", "customType": "goal", "content": "hidden" },
            { "role": "compactionSummary", "summary": "earlier work", "tokensBefore": 900 },
            { "role": "assistant", "content": [{ "type": "text", "text": "Everest." }] },
        ]);
        let updates: Vec<Value> = transcript_updates(messages.as_array().unwrap())
            .iter()
            .map(AcpSessionUpdate::to_bare_value)
            .collect();
        assert_eq!(
            updates,
            vec![
                json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "Name a river." } }),
                json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "and this" } }),
                json!({ "sessionUpdate": "user_message_chunk", "content": { "type": "image", "data": "AAAA", "mimeType": "image/png" } }),
                json!({ "sessionUpdate": "agent_thought_chunk", "messageId": "prime-agent-replay-assistant-1", "content": { "type": "text", "text": "rivers" } }),
                json!({ "sessionUpdate": "agent_message_chunk", "messageId": "prime-agent-replay-assistant-1", "content": { "type": "text", "text": "The Nile." } }),
                json!({ "sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "6*7", "kind": "execute", "status": "in_progress", "content": [{ "type": "content", "content": { "type": "text", "text": "```python\n6*7\n```" } }], "rawInput": { "code": "6*7" } }),
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "completed", "content": [{ "type": "content", "content": { "type": "text", "text": "```python\n6*7\n```" } }, { "type": "content", "content": { "type": "text", "text": "42" } }] }),
                json!({ "sessionUpdate": "tool_call", "toolCallId": "prime-agent-replay-bash-1", "title": "false", "kind": "execute", "status": "in_progress", "rawInput": { "command": "false" } }),
                json!({ "sessionUpdate": "tool_call_update", "toolCallId": "prime-agent-replay-bash-1", "status": "failed" }),
                json!({ "sessionUpdate": "session_info_update", "_meta": { "ai.primeintellect.prime-agent": { "compaction": { "tokensBefore": 900, "summary": "earlier work" } } } }),
                json!({ "sessionUpdate": "agent_message_chunk", "messageId": "prime-agent-replay-assistant-2", "content": { "type": "text", "text": "Everest." } }),
            ]
        );
    }

    #[test]
    fn a_costed_assistant_message_reports_the_context_usage() {
        let end = |message: Value| json!({ "type": "message_end", "message": message });
        let costed = end(json!({
            "role": "assistant",
            "stopReason": "stop",
            "usage": { "input": 20_000, "output": 846, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 20_846 },
        }));
        assert_eq!(
            usage_update(&costed, 1_000_000).map(|update| update.to_bare_value()),
            Some(json!({ "sessionUpdate": "usage_update", "used": 20_846, "size": 1_000_000 }))
        );
        // Unknown window, an errored or tokenless message, a user message,
        // or another event: nothing.
        assert!(usage_update(&costed, 0).is_none());
        assert!(usage_update(
            &end(
                json!({ "role": "assistant", "stopReason": "error", "usage": { "totalTokens": 5 } })
            ),
            1_000
        )
        .is_none());
        assert!(usage_update(
            &end(
                json!({ "role": "assistant", "stopReason": "stop", "usage": { "totalTokens": 0 } })
            ),
            1_000
        )
        .is_none());
        assert!(usage_update(
            &end(json!({ "role": "user", "usage": { "totalTokens": 5 } })),
            1_000
        )
        .is_none());
        assert!(usage_update(&json!({ "type": "agent_end" }), 1_000).is_none());
    }
}
