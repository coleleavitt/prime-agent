//! The `/harness` selector (#1118): open on the `/harness list` result,
//! toggle entries through `/harness enable|disable`, and fold every result
//! back (the rows stay off the transcript; outside the selector a result is
//! an ephemeral note).
use super::{key_event_to_id, AgentView, KeyEvent, Result, SessionUi, SubmitBehavior};
use crate::harness_selector::{
    toggle_command, HarnessResult, HarnessSelector, HarnessSelectorAction,
};
use crate::view::HarnessSelectorState;

impl SessionUi {
    /// `/harness` (or `/harness list`): the list request goes out and the
    /// selector opens when its result lands.
    pub(super) fn open_harness_selector(
        &mut self,
        behavior: SubmitBehavior,
        view: &mut AgentView,
    ) -> Result<()> {
        view.harness_selector = Some(HarnessSelectorState::Loading);
        self.send_prompt("/harness list", behavior, view)
    }

    /// One `/harness` result: the selector's list (or its refusal), or a note.
    pub(super) fn apply_harness_result(&mut self, result: HarnessResult, view: &mut AgentView) {
        self.dirty = true;
        match (view.harness_selector.take(), result.entries) {
            (Some(HarnessSelectorState::Loading), Some(entries)) if result.success => {
                if entries.is_empty() {
                    self.note(&result.text, view);
                } else {
                    view.harness_selector =
                        Some(HarnessSelectorState::Open(HarnessSelector::new(entries)));
                }
            }
            (Some(HarnessSelectorState::Open(mut selector)), entries) => {
                match entries {
                    Some(entries) if result.success => selector.refresh(entries),
                    _ => self.note_as(&result.text, crate::chat::StatusKind::Error, view),
                }
                view.harness_selector = Some(HarnessSelectorState::Open(selector));
            }
            (_, _) => {
                let kind = if result.success {
                    crate::chat::StatusKind::Info
                } else {
                    crate::chat::StatusKind::Error
                };
                self.note_as(&result.text, kind, view);
            }
        }
    }

    /// One key press while the selector is open.
    pub(super) fn handle_harness_selector_key(
        &mut self,
        key: KeyEvent,
        view: &mut AgentView,
    ) -> Result<()> {
        let Some(id) = key_event_to_id(&key) else {
            return Ok(());
        };
        // The selector consumes Ctrl+C (close, not exit).
        if id == "ctrl+c" {
            self.exit_guard.note_ctrl_c_handled();
        }
        let action = match view.harness_selector.as_mut() {
            Some(HarnessSelectorState::Open(selector)) => {
                selector.handle_key(&id, view.editor.keybindings())
            }
            _ => return Ok(()),
        };
        self.dirty = true;
        match action {
            HarnessSelectorAction::None => Ok(()),
            HarnessSelectorAction::Close => {
                view.harness_selector = None;
                Ok(())
            }
            HarnessSelectorAction::Toggle { key, enabled } => {
                self.send_prompt(&toggle_command(&key, enabled), SubmitBehavior::Steer, view)
            }
        }
    }
}
