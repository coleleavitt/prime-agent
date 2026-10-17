//! RLM child lifecycle notices: the durable custom rows a parent session receives when
//! a child run ends without an explicit reply, fails, or is cancelled; this module owns
//! the wire vocabulary, the content text, and the details block.
//!
//! The content is Claude Code's background-agent completion, a `<task-notification>`
//! (`<task-type>local_agent</task-type>`, rendered by
//! [`super::messages::task_notification`]): the parent reads one shape for every
//! background completion (commands, factory runs, agents). The custom types and
//! details keys are unchanged, so the transcript, the queue and the notice dedupe
//! read them as before.

use std::fmt::Write as _;

use pa_types::ai::UserContent;
use pa_types::session::CustomMessage;
use serde_json::json;

use super::agent_messaging::sanitize_message_header_value;
use super::messages::{TaskNotification, task_notification};

/// A child run that failed (TS `rlm_child_failure`).
pub const RLM_CHILD_FAILURE_CUSTOM_TYPE: &str = "rlm_child_failure";
/// A child run that ended without an explicit reply or was cancelled
/// (TS `rlm_child_terminal_notice`).
pub const RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE: &str = "rlm_child_terminal_notice";
/// The `<task-type>` of a child agent's notification (Claude Code's
/// `local_agent`).
pub const RLM_CHILD_TASK_TYPE: &str = "local_agent";

/// The note a no-reply completion carries: where the full answer is and
/// how to continue the child.
const NO_REPLY_NOTE: &str = "The agent ended its task without replying through \
    agent_message.send; the result is its last assistant text. rlm.collect reads the full \
    answer, and rlm.send_message continues the agent in the same context.";

/// The parent generation and child identity are immutable through rebind.
#[must_use]
pub fn terminal_notice_key(message: &CustomMessage) -> Option<&str> {
    if message.custom_type != RLM_CHILD_FAILURE_CUSTOM_TYPE
        && message.custom_type != RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE
    {
        return None;
    }
    message.details.as_ref()?.get("noticeKey")?.as_str()
}

/// How a child run ended without an explicit reply (TS
/// `RlmChildTerminalNoticeDetails`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RlmChildTerminalNotice {
    /// The parent deleted a still-running child.
    Cancelled {
        child_id: String,
        session_name: String,
        reason: Option<String>,
    },
    /// The child finished its task without sending an agent message back.
    CompletedWithoutReply {
        child_id: String,
        session_name: String,
        last_assistant_text_preview: Option<String>,
    },
}

/// What a host knows about a child's run beyond its outcome: Claude Code's
/// `<usage>` and `<worktree>` elements.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RlmChildNoticeExtras {
    /// The run's wall time.
    pub duration_ms: Option<u64>,
    /// The worktree the child ran in, when it was isolated in one.
    pub worktree: Option<crate::git_worktree::GitWorktree>,
}

impl RlmChildNoticeExtras {
    /// The trailer lines: `<usage>` and `<worktree>`, each when known.
    fn trailer(&self) -> String {
        let mut lines = String::new();
        if let Some(duration_ms) = self.duration_ms {
            let _ = writeln!(
                lines,
                "<usage><duration_ms>{duration_ms}</duration_ms></usage>"
            );
        }
        if let Some(worktree) = &self.worktree {
            let _ = writeln!(
                lines,
                "<worktree><worktreePath>{}</worktreePath><worktreeBranch>{}</worktreeBranch></worktree>",
                worktree.path.display(),
                worktree.branch
            );
        }
        lines
    }

    /// The details keys the extras add.
    fn extend_details(&self, details: &mut serde_json::Value) {
        if let Some(duration_ms) = self.duration_ms {
            details["durationMs"] = json!(duration_ms);
        }
        if let Some(worktree) = &self.worktree {
            details["worktreePath"] = json!(worktree.path.display().to_string());
            details["worktreeBranch"] = json!(worktree.branch);
        }
    }
}

/// One child notification's text.
fn child_notification(
    child_id: &str,
    status: &str,
    summary: &str,
    note: Option<&str>,
    result: Option<&str>,
    extras: &RlmChildNoticeExtras,
) -> String {
    let trailer = extras.trailer();
    task_notification(&TaskNotification {
        task_id: child_id,
        task_type: RLM_CHILD_TASK_TYPE,
        status,
        summary,
        tool_use_id: None,
        output_file: None,
        note,
        result,
        trailer: Some(&trailer),
    })
}

/// The terminal-notice row without run extras (see
/// [`create_rlm_child_terminal_notice_with`]).
#[must_use]
pub fn create_rlm_child_terminal_notice(
    notice: &RlmChildTerminalNotice,
    timestamp: u64,
) -> CustomMessage {
    create_rlm_child_terminal_notice_with(notice, &RlmChildNoticeExtras::default(), timestamp)
}

/// The terminal-notice row: a `<task-notification>` whose status is
/// `completed` (no reply; the child's last assistant text is the result)
/// or `cancelled` (the reason is the result).
#[must_use]
pub fn create_rlm_child_terminal_notice_with(
    notice: &RlmChildTerminalNotice,
    extras: &RlmChildNoticeExtras,
    timestamp: u64,
) -> CustomMessage {
    let (content, mut details) = match notice {
        RlmChildTerminalNotice::Cancelled {
            child_id,
            session_name,
            reason,
        } => {
            let name = sanitize_message_header_value(session_name);
            let summary = format!("Agent \"{name}\" was cancelled");
            let content = child_notification(
                child_id,
                "cancelled",
                &summary,
                None,
                reason.as_deref(),
                extras,
            );
            let details = json!({
                "kind": "cancelled",
                "childId": child_id,
                "sessionName": session_name,
                "reason": reason,
                "taskType": RLM_CHILD_TASK_TYPE,
                "status": "cancelled",
                "summary": summary,
            });
            (content, details)
        }
        RlmChildTerminalNotice::CompletedWithoutReply {
            child_id,
            session_name,
            last_assistant_text_preview,
        } => {
            let name = sanitize_message_header_value(session_name);
            let summary = format!("Agent \"{name}\" finished without replying");
            let content = child_notification(
                child_id,
                "completed",
                &summary,
                Some(NO_REPLY_NOTE),
                last_assistant_text_preview.as_deref(),
                extras,
            );
            let details = json!({
                "kind": "completed_without_reply",
                "childId": child_id,
                "sessionName": session_name,
                "lastAssistantTextPreview": last_assistant_text_preview,
                "taskType": RLM_CHILD_TASK_TYPE,
                "status": "completed",
                "summary": summary,
            });
            (content, details)
        }
    };
    extras.extend_details(&mut details);
    CustomMessage {
        custom_type: RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE.to_string(),
        content: UserContent::Text(content),
        display: true,
        details: Some(details),
        timestamp,
        rest: serde_json::Map::default(),
    }
}

/// The failure row without run extras (see
/// [`create_rlm_child_failure_message_with`]).
#[must_use]
pub fn create_rlm_child_failure_message(
    child_id: &str,
    session_name: &str,
    error: &str,
    timestamp: u64,
) -> CustomMessage {
    create_rlm_child_failure_message_with(
        child_id,
        session_name,
        error,
        &RlmChildNoticeExtras::default(),
        timestamp,
    )
}

/// The failure row: a `failed` `<task-notification>` with the error text as
/// its result.
#[must_use]
pub fn create_rlm_child_failure_message_with(
    child_id: &str,
    session_name: &str,
    error: &str,
    extras: &RlmChildNoticeExtras,
    timestamp: u64,
) -> CustomMessage {
    let name = sanitize_message_header_value(session_name);
    let summary = format!("Agent \"{name}\" failed");
    let content = child_notification(child_id, "failed", &summary, None, Some(error), extras);
    let mut details = json!({
        "childId": child_id,
        "sessionName": session_name,
        "error": error,
        "taskType": RLM_CHILD_TASK_TYPE,
        "status": "failed",
        "summary": summary,
    });
    extras.extend_details(&mut details);
    CustomMessage {
        custom_type: RLM_CHILD_FAILURE_CUSTOM_TYPE.to_string(),
        content: UserContent::Text(content),
        display: true,
        details: Some(details),
        timestamp,
        rest: serde_json::Map::default(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn text(message: &CustomMessage) -> &str {
        match &message.content {
            UserContent::Text(text) => text,
            UserContent::Blocks(_) => panic!("notice content is text"),
        }
    }

    #[test]
    fn no_reply_notice_is_an_agent_task_notification() {
        let notice = RlmChildTerminalNotice::CompletedWithoutReply {
            child_id: "sub-8fb5284a".to_string(),
            session_name: "f20-worker".to_string(),
            last_assistant_text_preview: Some("done with the task".to_string()),
        };
        let message = create_rlm_child_terminal_notice(&notice, 1_000);
        assert_eq!(message.custom_type, "rlm_child_terminal_notice");
        assert!(message.display);
        assert_eq!(
            text(&message),
            format!(
                "<task-notification>\n<task-id>sub-8fb5284a</task-id>\n<task-type>local_agent</task-type>\n\
                 <status>completed</status>\n<summary>Agent \"f20-worker\" finished without replying</summary>\n\
                 <note>{NO_REPLY_NOTE}</note>\n<result>done with the task</result>\n</task-notification>"
            )
        );
        assert_eq!(
            message.details.unwrap(),
            json!({
                "kind": "completed_without_reply",
                "childId": "sub-8fb5284a",
                "sessionName": "f20-worker",
                "lastAssistantTextPreview": "done with the task",
                "taskType": "local_agent",
                "status": "completed",
                "summary": "Agent \"f20-worker\" finished without replying",
            })
        );
    }

    #[test]
    fn cancelled_notice_without_reason_has_no_result() {
        let notice = RlmChildTerminalNotice::Cancelled {
            child_id: "sub-1".to_string(),
            session_name: "worker [x]".to_string(),
            reason: None,
        };
        let message = create_rlm_child_terminal_notice(&notice, 2_000);
        assert_eq!(
            text(&message),
            "<task-notification>\n<task-id>sub-1</task-id>\n<task-type>local_agent</task-type>\n\
             <status>cancelled</status>\n<summary>Agent \"worker x\" was cancelled</summary>\n\
             </task-notification>"
        );
        let details = message.details.unwrap();
        assert_eq!(details["kind"], "cancelled");
        assert_eq!(details["reason"], serde_json::Value::Null);
    }

    #[test]
    fn extras_render_usage_and_the_worktree() {
        let extras = RlmChildNoticeExtras {
            duration_ms: Some(1_500),
            worktree: Some(crate::git_worktree::GitWorktree {
                path: PathBuf::from("/r/.prime/worktrees/agent-1"),
                branch: "worktree-agent-1".to_string(),
                repo_root: PathBuf::from("/r"),
                base_commit: "abc".to_string(),
            }),
        };
        let message =
            create_rlm_child_failure_message_with("sub-9", "lane", "boom", &extras, 4_000);
        assert_eq!(message.custom_type, "rlm_child_failure");
        assert_eq!(
            text(&message),
            "<task-notification>\n<task-id>sub-9</task-id>\n<task-type>local_agent</task-type>\n\
             <status>failed</status>\n<summary>Agent \"lane\" failed</summary>\n<result>boom</result>\n\
             <usage><duration_ms>1500</duration_ms></usage>\n\
             <worktree><worktreePath>/r/.prime/worktrees/agent-1</worktreePath>\
             <worktreeBranch>worktree-agent-1</worktreeBranch></worktree>\n\
             </task-notification>"
        );
        assert_eq!(
            message.details.unwrap(),
            json!({
                "childId": "sub-9",
                "sessionName": "lane",
                "error": "boom",
                "taskType": "local_agent",
                "status": "failed",
                "summary": "Agent \"lane\" failed",
                "durationMs": 1_500,
                "worktreePath": "/r/.prime/worktrees/agent-1",
                "worktreeBranch": "worktree-agent-1",
            })
        );
    }
}
