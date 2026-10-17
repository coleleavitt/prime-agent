//! `/rewind` and Esc Esc: the picker over the session's prompts and the
//! restore it confirms (the daemon's `get_rewind_checkpoints` / `rewind`,
//! capability `rewind`).

use super::{
    AgentView,
    DaemonCommand,
    Duration,
    KeyEvent,
    Map,
    Result,
    SessionUi,
    UI_REQUEST_TIMEOUT_MS,
    Value,
    key_event_to_id,
};
use crate::rewind_selector::{
    RewindCheckpoint,
    RewindOption,
    RewindSelector,
    RewindSelectorAction,
};

/// The daemon capability behind `/rewind`.
const REWIND_CAPABILITY: &str = "rewind";

impl SessionUi {
    /// Open the `/rewind` picker; a daemon without it gets the `/tree`
    /// selector, the closest navigation it has.
    pub(super) async fn open_rewind_or_tree(&mut self, view: &mut AgentView) -> Result<()> {
        if !self.client.supports_server_capability(REWIND_CAPABILITY) {
            return self.open_tree_selector(view, None).await;
        }
        let data = match self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetRewindCheckpoints {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    rest: Map::default(),
                },
            )
            .await
        {
            Ok(data) => data,
            Err(error) => {
                self.note(&format!("rewind unavailable: {error:#}"), view);
                return Ok(());
            }
        };
        view.rewind_selector = Some(RewindSelector::new(RewindCheckpoint::from_response(&data)));
        self.dirty = true;
        Ok(())
    }

    /// One key press while the picker is open.
    pub(super) async fn handle_rewind_selector_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        let action = {
            let Some(selector) = view.rewind_selector.as_mut() else {
                return Ok(());
            };
            let kb = view.editor.keybindings();
            selector.handle_key(kb, &id)
        };
        match action {
            RewindSelectorAction::None => {}
            RewindSelectorAction::Cancel => view.rewind_selector = None,
            RewindSelectorAction::Rewind { entry_id, option } => {
                view.rewind_selector = None;
                self.rewind(&entry_id, option, view).await?;
            }
        }
        self.dirty = true;
        Ok(())
    }

    async fn rewind(
        &mut self,
        entry_id: &str,
        option: RewindOption,
        view: &mut AgentView,
    ) -> Result<()> {
        let (restore_code, restore_conversation, summarize) = match option {
            RewindOption::Both => (true, true, false),
            RewindOption::Conversation => (false, true, false),
            RewindOption::Code => (true, false, false),
            RewindOption::Summarize => (false, false, true),
            RewindOption::Nevermind => return Ok(()),
        };
        if summarize {
            self.note("Summarizing\u{2026}", view);
        }
        let result = self
            .bounded_request(
                // A summary is a model call, like a tree move's.
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS * 3),
                DaemonCommand::Rewind {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    entry_id: entry_id.to_string(),
                    restore_code: Some(restore_code),
                    restore_conversation: Some(restore_conversation),
                    summarize: Some(summarize),
                    custom_instructions: None,
                    force: None,
                    rest: Map::default(),
                },
            )
            .await;
        let data = match result {
            Ok(data) => data,
            Err(error) => {
                self.note(&format!("{error:#}"), view);
                return Ok(());
            }
        };
        if let Some(conversation) = data.get("conversation") {
            if conversation.get("aborted").and_then(Value::as_bool) == Some(true) {
                self.note("Summarization cancelled", view);
                return Ok(());
            }
            self.rebuild_transcript(view).await;
            // Claude Code puts the prompt back in the input to edit and resend.
            if restore_conversation {
                if let Some(text) = conversation.get("editorText").and_then(Value::as_str) {
                    if view.editor.get_text().trim().is_empty() {
                        view.editor.set_text(text);
                    }
                }
            }
        }
        // One status row: a later note replaces an earlier one.
        let notes = rewind_notes(&data, restore_conversation, summarize);
        if !notes.is_empty() {
            self.note(&notes.join("; "), view);
        }
        self.dirty = true;
        Ok(())
    }
}

/// The status lines a finished rewind leaves: what the code restore did
/// (and skipped, with why), then the conversation's move.
pub(super) fn rewind_notes(data: &Value, conversation: bool, summarize: bool) -> Vec<String> {
    let mut notes = Vec::new();
    let paths = |code: &Value, key: &str| -> usize {
        code.get(key).and_then(Value::as_array).map_or(0, Vec::len)
    };
    if let Some(code) = data.get("code") {
        let changed = paths(code, "restored") + paths(code, "deleted");
        notes.push(match changed {
            0 => "The code has not changed (nothing was restored)".to_string(),
            1 => "Restored the code: 1 file".to_string(),
            count => format!("Restored the code: {count} files"),
        });
        for skipped in code
            .get("skipped")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let path = skipped.get("path").and_then(Value::as_str).unwrap_or("?");
            let reason = skipped.get("reason").and_then(Value::as_str).unwrap_or("");
            notes.push(format!("Skipped {path}: {reason}"));
        }
    }
    if let Some(error) = data.get("codeError").and_then(Value::as_str) {
        notes.push(format!("Failed to restore the code: {error}"));
    }
    if data.get("conversation").is_some() {
        notes.push(if summarize {
            "Summarized the conversation from the selected point".to_string()
        } else if conversation {
            "Restored the conversation to before the selected message".to_string()
        } else {
            "Moved to the selected point".to_string()
        });
    }
    notes
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::rewind_notes;

    #[test]
    fn the_notes_report_the_code_and_the_conversation() {
        let data = json!({
            "code": {
                "restored": ["/w/a.rs"],
                "deleted": ["/w/new.rs"],
                "skipped": [{ "path": "/w/hand.rs", "reason": "changed outside the agent since its last edit" }],
            },
            "conversation": { "cancelled": false, "editorText": "second prompt" },
        });
        assert_eq!(
            rewind_notes(&data, true, false),
            vec![
                "Restored the code: 2 files".to_string(),
                "Skipped /w/hand.rs: changed outside the agent since its last edit".to_string(),
                "Restored the conversation to before the selected message".to_string(),
            ]
        );
        assert_eq!(
            rewind_notes(
                &json!({ "codeError": "No file checkpoints for this session", "conversation": {} }),
                true,
                false
            ),
            vec![
                "Failed to restore the code: No file checkpoints for this session".to_string(),
                "Restored the conversation to before the selected message".to_string(),
            ]
        );
        assert_eq!(
            rewind_notes(
                &json!({ "conversation": { "summaryEntry": {} } }),
                false,
                true
            ),
            vec!["Summarized the conversation from the selected point".to_string()]
        );
        assert_eq!(
            rewind_notes(
                &json!({ "code": { "restored": [], "deleted": [], "skipped": [] } }),
                false,
                false
            ),
            vec!["The code has not changed (nothing was restored)".to_string()]
        );
    }
}
