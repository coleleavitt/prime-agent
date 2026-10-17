//! Nested instruction files at the turn boundary (Claude Code's nested
//! memory): after a tool batch, the files its cells opened
//! (`accessedPaths` on the `python_repl` result details) may bring in the
//! instruction files of directories below the working directory and the
//! path-scoped rules they match. Those join the running run as one hidden
//! row, delivered through the steering queue like a feature's context
//! (`feature_boundary`), so the model reads them before its next request.
//!
//! The row's details name the delivered files, so a resumed session does
//! not deliver them again ([`delivered_paths`]).

use std::path::PathBuf;

use pa_agent::types::{AgentMessage, ToolResultMessage};

use super::steering_fold::FoldAdmissionSlot;
use super::{SessionAgentMessage, session_message_to_loop};
use crate::resources::instructions::{NestedInstructions, render_delivered};

/// The custom type of the hidden row carrying nested instruction files.
pub const NESTED_INSTRUCTIONS_CUSTOM_TYPE: &str = "nested_instructions";

/// The project files a finished tool batch touched, in call order.
#[must_use]
pub(super) fn touched_paths(tool_results: &[ToolResultMessage]) -> Vec<PathBuf> {
    tool_results
        .iter()
        .filter_map(|result| result.details.as_ref())
        .filter_map(|details| details.get("accessedPaths"))
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(PathBuf::from)
        .collect()
}

/// The hidden row for newly delivered files, or `None` when the touched
/// paths bring nothing new.
#[must_use]
pub(super) fn nested_row(
    nested: &NestedInstructions,
    touched: &[PathBuf],
) -> Option<pa_types::session::CustomMessage> {
    let files = nested.touch(touched);
    if files.is_empty() {
        return None;
    }
    let mut row =
        super::plan_mode::hidden_row(NESTED_INSTRUCTIONS_CUSTOM_TYPE, &render_delivered(&files));
    row.details = Some(serde_json::json!({
        "paths": files
            .iter()
            .map(|file| file.path.display().to_string())
            .collect::<Vec<_>>(),
        "kinds": files.iter().map(|file| file.kind.as_str()).collect::<Vec<_>>(),
    }));
    Some(row)
}

/// Deliver the files a finished batch brings in to the running run.
pub(super) fn after_turn(
    slot: &FoldAdmissionSlot,
    nested: &NestedInstructions,
    touched: &[PathBuf],
) {
    if touched.is_empty() {
        return;
    }
    let Some(agent) = slot.get().and_then(|admission| admission.agent.upgrade()) else {
        return;
    };
    let Some(row) = nested_row(nested, touched) else {
        return;
    };
    if let Some(row) = session_message_to_loop(&SessionAgentMessage::Custom(row)) {
        agent.steer(row);
    }
}

/// The files earlier nested rows of a conversation delivered.
#[must_use]
pub fn delivered_paths(messages: &[AgentMessage]) -> Vec<PathBuf> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::Custom(row)
                if row
                    .payload
                    .get("customType")
                    .and_then(serde_json::Value::as_str)
                    == Some(NESTED_INSTRUCTIONS_CUSTOM_TYPE) =>
            {
                row.payload.get("details")
            }
            _ => None,
        })
        .filter_map(|details| details.get("paths"))
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::instructions::{InstructionOptions, load_instructions};

    fn tool_result(details: serde_json::Value) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: pa_types::ai::PYTHON_TOOL_NAME.to_string(),
            content: Vec::new(),
            details: Some(details),
            is_error: false,
            timestamp: 0,
        }
    }

    #[test]
    fn touched_paths_read_every_result_in_order() {
        let results = vec![
            tool_result(serde_json::json!({ "accessedPaths": ["/r/a", "/r/b"] })),
            tool_result(serde_json::json!({ "status": "ok" })),
            tool_result(serde_json::json!({ "accessedPaths": ["/r/c", 3] })),
        ];
        assert_eq!(
            touched_paths(&results),
            vec![
                PathBuf::from("/r/a"),
                PathBuf::from("/r/b"),
                PathBuf::from("/r/c")
            ]
        );
    }

    #[test]
    fn a_nested_row_carries_the_files_once_and_seeds_a_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("pkg")).unwrap();
        std::fs::write(repo.join("pkg").join("AGENTS.md"), "pkg rules").unwrap();
        let options = InstructionOptions {
            cwd: repo.clone(),
            agent_dir: root.join("agent"),
            home_dir: None,
            claude_user_files: false,
        };
        let (_, nested) = load_instructions(options.clone());
        let touched = vec![repo.join("pkg").join("lib.py")];
        let row = nested_row(&nested, &touched).expect("the nested file");
        assert_eq!(row.custom_type, NESTED_INSTRUCTIONS_CUSTOM_TYPE);
        assert!(!row.display);
        let pkg_agents = repo.join("pkg").join("AGENTS.md");
        assert_eq!(
            row.details,
            Some(serde_json::json!({
                "paths": [pkg_agents.display().to_string()],
                "kinds": ["project"],
            }))
        );
        assert_eq!(
            row.content,
            pa_types::ai::UserContent::Text(format!(
                "Contents of {} (project instructions, checked into the codebase):\n\npkg rules",
                pkg_agents.display()
            ))
        );
        assert!(nested_row(&nested, &touched).is_none(), "delivered once");

        // A resumed session reads the delivered files back from its rows.
        let loop_row =
            session_message_to_loop(&SessionAgentMessage::Custom(row)).expect("loop form");
        let delivered = delivered_paths(&[loop_row]);
        assert_eq!(delivered, vec![pkg_agents]);
        let (_, resumed) = load_instructions(options);
        resumed.mark_delivered(&delivered);
        assert!(nested_row(&resumed, &touched).is_none());
    }
}
