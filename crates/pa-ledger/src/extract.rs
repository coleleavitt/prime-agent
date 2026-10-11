//! Failure observation over session messages (TS `extractFailures`): Python
//! tracebacks in tool output, tool results flagged `isError`, and assistant
//! messages that ended with `stopReason: "error"`.

use pa_agent::types::{
    AgentMessage,
    AssistantMessage,
    Message,
    StopReason,
    ToolResultContent,
    ToolResultMessage,
};
use serde_json::Value;

use crate::fingerprint::{
    FailureFingerprint,
    FailureKind,
    ParsedTraceback,
    clip_excerpt,
    fingerprint_failure,
    parse_python_traceback,
    tool_error_text,
};
use crate::js::js_trim;
use crate::ledger::FailureObservation;
use crate::replay::derive_replay_case;

/// The kernel tool: its error details carry the traceback of the exception a
/// cell itself raised.
pub const IPYTHON_TOOL_NAME: &str = "ipython";

/// A tool result's text parts joined by newlines (TS `messageText`).
#[must_use]
pub fn tool_result_text(content: &[ToolResultContent]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `Some(name)` unless the name is empty (TS drops a falsy `provider`).
fn named(name: &str) -> Option<&str> {
    (!name.is_empty()).then_some(name)
}

/// The traceback of the exception an `ipython` cell raised, read from the
/// kernel's error details, and only when it fingerprints the same failure
/// the text does: a traceback a cell merely printed may never name what a
/// replay case imports.
fn kernel_cell_traceback(
    message: &ToolResultMessage,
    fingerprint: &FailureFingerprint,
) -> Option<ParsedTraceback> {
    if message.tool_name != IPYTHON_TOOL_NAME {
        return None;
    }
    let details = message.details.as_ref()?.as_object()?;
    if details.get("status").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let error = details.get("error")?.as_object()?;
    let ename = error.get("ename")?.as_str()?;
    let lines = error.get("traceback")?.as_array()?;
    let lines: Vec<&str> = lines.iter().map(Value::as_str).collect::<Option<_>>()?;
    let parsed = parse_python_traceback(&lines.join("\n"))?;
    if parsed.exception_class.rsplit('.').next() != Some(ename) {
        return None;
    }
    let kernel = fingerprint_failure(
        FailureKind::PythonException,
        parsed
            .skill_name
            .as_deref()
            .or(Some(message.tool_name.as_str())),
        Some(&parsed.exception_class),
        &parsed.message,
    );
    (kernel.id == fingerprint.id).then_some(parsed)
}

fn observe_tool_result(
    message: &ToolResultMessage,
    entry_index: u64,
    turn: u64,
    now: &dyn Fn() -> String,
) -> Option<FailureObservation> {
    let text = tool_result_text(&message.content);
    // TS passes `message.toolName` as is: an empty name is a source of "".
    let tool_name = Some(message.tool_name.as_str());
    if let Some(traceback) = parse_python_traceback(&text) {
        let fingerprint = fingerprint_failure(
            FailureKind::PythonException,
            traceback.skill_name.as_deref().or(tool_name),
            Some(&traceback.exception_class),
            &traceback.message,
        );
        let replay_case = kernel_cell_traceback(message, &fingerprint)
            .and_then(|kernel| derive_replay_case(&fingerprint, &kernel.excerpt));
        return Some(FailureObservation {
            fingerprint,
            excerpt: traceback.excerpt,
            entry_index,
            turn,
            at: now(),
            replay_case,
        });
    }
    if !message.is_error {
        return None;
    }
    let raw = tool_error_text(&text);
    Some(FailureObservation {
        fingerprint: fingerprint_failure(FailureKind::ToolError, tool_name, None, raw),
        excerpt: clip_excerpt(raw),
        entry_index,
        turn,
        at: now(),
        replay_case: None,
    })
}

fn observe_assistant(
    message: &AssistantMessage,
    entry_index: u64,
    turn: u64,
    now: &dyn Fn() -> String,
) -> Option<FailureObservation> {
    if message.stop_reason != StopReason::Error {
        return None;
    }
    let trimmed = message
        .error_message
        .as_deref()
        .map(js_trim)
        .unwrap_or_default();
    let raw = if trimmed.is_empty() {
        message
            .stop_reason_raw
            .as_deref()
            .filter(|raw| !raw.is_empty())
            .unwrap_or("provider returned an error")
    } else {
        trimmed
    };
    Some(FailureObservation {
        fingerprint: fingerprint_failure(
            FailureKind::ProviderError,
            named(&message.provider),
            None,
            raw,
        ),
        excerpt: clip_excerpt(raw),
        entry_index,
        turn,
        at: now(),
        replay_case: None,
    })
}

/// The failure one message records, if any.
#[must_use]
pub fn observe_message(
    message: &AgentMessage,
    entry_index: u64,
    turn: u64,
    now: &dyn Fn() -> String,
) -> Option<FailureObservation> {
    match message {
        AgentMessage::Standard(Message::ToolResult(result)) => {
            observe_tool_result(result, entry_index, turn, now)
        }
        AgentMessage::Standard(Message::Assistant(assistant)) => {
            observe_assistant(assistant, entry_index, turn, now)
        }
        AgentMessage::Standard(Message::User(_)) | AgentMessage::Custom(_) => None,
    }
}

/// Every failure in `messages[from_entry_index..]`, each attributed to `turn`.
#[must_use]
pub fn extract_failures(
    messages: &[AgentMessage],
    from_entry_index: u64,
    turn: u64,
    now: &dyn Fn() -> String,
) -> Vec<FailureObservation> {
    let start = usize::try_from(from_entry_index).unwrap_or(usize::MAX);
    messages
        .iter()
        .enumerate()
        .skip(start)
        .filter_map(|(index, message)| observe_message(message, index as u64, turn, now))
        .collect()
}
