//! Brief mode's collapsed assistant text. Claude Code's brief view keeps a
//! turn's plain assistant text out of sight once the turn sent the user a
//! message (`3gafw66q:tS`/`eS`: the message carries the answer; the text is
//! there in the detail view). Here, while the session's `brief` status is
//! on, the text blocks of every assistant entry in a turn (the entries
//! between two user prompts) that holds a `rlm.send_user_message` row are
//! collapsed below the `all` detail level (Ctrl+O shows them). Thinking,
//! tool cards and error rows keep their usual rules.
//!
//! `brief_hidden` holds the collapsed indices (shifted, popped and cleared
//! like `toggled_cards`); a change re-lays the entry out through the
//! in-place-mutation path.

use std::borrow::Cow;

use super::AgentView;
use crate::chat::{AssistantMessage, ChatEntry, Detail, MessageBlock};

impl AgentView {
    /// Switch brief mode (the session's `brief` feature status); every
    /// turn's collapsing follows.
    pub(crate) fn set_brief_mode(&mut self, on: bool) {
        if self.brief_mode == on {
            return;
        }
        self.brief_mode = on;
        self.refresh_brief_hidden();
    }

    /// The assistant message an entry lays out: its text blocks drop while
    /// brief mode collapses them and the level is below `all`.
    pub(super) fn assistant_layout<'a>(
        &self,
        index: usize,
        message: &'a AssistantMessage,
        detail: Detail,
    ) -> Cow<'a, AssistantMessage> {
        if detail.tool_output_expanded() || !self.brief_hidden.contains(&index) {
            return Cow::Borrowed(message);
        }
        let mut collapsed = message.clone();
        collapsed
            .blocks
            .retain(|block| matches!(block, MessageBlock::Thinking(_)));
        Cow::Owned(collapsed)
    }

    /// The assistant entries brief mode collapses: those of every turn
    /// holding a brief message (none while brief mode is off).
    fn brief_hidden_target(&self) -> std::collections::BTreeSet<usize> {
        let mut hidden = std::collections::BTreeSet::new();
        if !self.brief_mode {
            return hidden;
        }
        let mut turn_start = 0;
        for end in 0..=self.chat.len() {
            let boundary =
                end == self.chat.len() || matches!(self.chat[end], ChatEntry::User { .. });
            if !boundary {
                continue;
            }
            let turn = &self.chat[turn_start..end];
            if turn
                .iter()
                .any(|entry| matches!(entry, ChatEntry::BriefMessage(_)))
            {
                hidden.extend(
                    turn.iter()
                        .enumerate()
                        .filter(|(_, entry)| matches!(entry, ChatEntry::Assistant(_)))
                        .map(|(offset, _)| turn_start + offset),
                );
            }
            turn_start = end;
        }
        hidden
    }

    /// Bring `brief_hidden` up to date, re-laying out each entry that
    /// changed.
    pub(super) fn refresh_brief_hidden(&mut self) {
        let target = self.brief_hidden_target();
        let changed: Vec<usize> = self
            .brief_hidden
            .symmetric_difference(&target)
            .copied()
            .collect();
        for index in changed {
            self.prepare_entry_mutation(index);
            if !self.brief_hidden.remove(&index) {
                self.brief_hidden.insert(index);
            }
            self.mark_entry_stale(index);
        }
    }

    /// An appended entry may join (or open) a turn with a brief message.
    pub(super) fn brief_note_append(&mut self) {
        if !self.brief_mode {
            return;
        }
        if matches!(
            self.chat.last(),
            Some(ChatEntry::BriefMessage(_) | ChatEntry::Assistant(_))
        ) {
            self.refresh_brief_hidden();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom_message::BriefMessageRow;
    use crate::theme::{ColorMode, Theme};

    fn assistant(text: &str) -> ChatEntry {
        ChatEntry::Assistant(Box::new(AssistantMessage {
            blocks: vec![MessageBlock::Text(text.to_string())],
            has_tool_calls: false,
            streaming: false,
            error: None,
            aborted: false,
        }))
    }

    fn brief(text: &str) -> ChatEntry {
        ChatEntry::BriefMessage(Box::new(BriefMessageRow {
            message: text.to_string(),
            proactive: false,
            attachments: Vec::new(),
        }))
    }

    fn user(text: &str) -> ChatEntry {
        ChatEntry::User {
            text: text.to_string(),
        }
    }

    fn transcript(view: &mut AgentView) -> String {
        super::super::expansion::tests::transcript_text(view)
    }

    #[test]
    fn a_turn_with_a_message_collapses_its_assistant_text_in_brief_mode() {
        let mut view = AgentView::new(Theme::builtin("prime", ColorMode::TrueColor));
        view.push_entry(user("first question"));
        view.push_entry(assistant("plain narration one"));
        view.push_entry(brief("the real answer"));
        view.push_entry(assistant("trailing narration"));
        view.push_entry(user("second question"));
        view.push_entry(assistant("plain answer two"));
        let off = transcript(&mut view);
        for text in [
            "plain narration one",
            "the real answer",
            "trailing narration",
            "plain answer two",
        ] {
            assert!(off.contains(text), "brief mode off shows {text:?}: {off}");
        }

        view.set_brief_mode(true);
        let on = transcript(&mut view);
        assert!(on.contains("\u{25cf} Message"));
        assert!(on.contains("the real answer"));
        assert!(!on.contains("plain narration one"), "{on}");
        assert!(!on.contains("trailing narration"), "{on}");
        assert!(
            on.contains("plain answer two"),
            "a turn without a message keeps its text: {on}"
        );

        // A message arriving later collapses its turn's earlier text.
        view.push_entry(brief("answer two"));
        let later = transcript(&mut view);
        assert!(!later.contains("plain answer two"), "{later}");

        // The `all` level shows everything, and leaving brief mode restores it.
        view.cycle_detail();
        view.cycle_detail();
        assert!(transcript(&mut view).contains("plain narration one"));
        view.cycle_detail();
        view.set_brief_mode(false);
        let restored = transcript(&mut view);
        assert!(restored.contains("plain narration one") && restored.contains("plain answer two"));
    }

    #[test]
    fn collapsing_survives_pops_prepends_and_clears() {
        let mut view = AgentView::new(Theme::builtin("prime", ColorMode::TrueColor));
        view.set_brief_mode(true);
        view.push_entry(assistant("narration"));
        view.push_entry(brief("answer"));
        assert_eq!(view.brief_hidden.iter().copied().collect::<Vec<_>>(), [0]);
        view.prepend_entries(vec![
            user("older"),
            assistant("older text"),
            user("current"),
        ]);
        assert_eq!(view.brief_hidden.iter().copied().collect::<Vec<_>>(), [3]);
        let _ = view.pop_chat_entry();
        assert!(
            view.brief_hidden.is_empty(),
            "the message left with its pop"
        );
        view.push_entry(brief("again"));
        view.clear_chat();
        assert!(view.brief_hidden.is_empty());
    }
}
