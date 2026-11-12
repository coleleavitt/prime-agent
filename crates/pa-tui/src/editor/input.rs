//! Editor key dispatch: decodes TS-style key ids to editor actions through the
//! user's keybindings.

use super::text_utils::{char_at, decode_printable};
use super::{Editor, EditorEvent, JumpDirection};

/// One editor key: the keybinding action id and what the key does. A table of these is tried in
/// order and the first matching binding runs.
type EditorKey = (&'static str, fn(&mut Editor));

/// The keys dispatched before the completion menu: undo/redo, the clipboard, and the selection
/// families (standard editors' shift+arrow set; no TS counterpart — see selection.rs). Every plain
/// motion below collapses the selection, so these run first. Selection motions move the cursor
/// away from the completion token: the dropdown closes instead of holding a stale anchor for the
/// next Enter.
const SELECTION_KEYS: &[EditorKey] = &[
    ("tui.input.copy", |_| {}),
    ("tui.editor.undo", Editor::undo),
    ("tui.editor.redo", Editor::redo),
    ("tui.editor.selectAll", Editor::select_all),
    ("tui.editor.cutSelection", |editor| {
        if let Some(text) = editor.cut_selection() {
            editor.emit(EditorEvent::ClipboardWrite(text));
        }
    }),
    ("tui.editor.copySelection", |editor| {
        if let Some(text) = editor.copy_selection() {
            editor.emit(EditorEvent::ClipboardWrite(text));
        }
    }),
    ("tui.editor.transposeChars", Editor::transpose_chars),
    ("tui.editor.selectLeft", |editor| {
        editor.cancel_autocomplete();
        editor.select_left();
    }),
    ("tui.editor.selectRight", |editor| {
        editor.cancel_autocomplete();
        editor.select_right();
    }),
    ("tui.editor.selectUp", |editor| {
        editor.cancel_autocomplete();
        editor.select_up();
    }),
    ("tui.editor.selectDown", |editor| {
        editor.cancel_autocomplete();
        editor.select_down();
    }),
    ("tui.editor.selectWordLeft", |editor| {
        editor.cancel_autocomplete();
        editor.select_word_left();
    }),
    ("tui.editor.selectWordRight", |editor| {
        editor.cancel_autocomplete();
        editor.select_word_right();
    }),
    ("tui.editor.selectLineStart", |editor| {
        editor.cancel_autocomplete();
        editor.select_line_start();
    }),
    ("tui.editor.selectLineEnd", |editor| {
        editor.cancel_autocomplete();
        editor.select_line_end();
    }),
    ("tui.editor.selectParagraphUp", |editor| {
        editor.cancel_autocomplete();
        editor.select_paragraph_up();
    }),
    ("tui.editor.selectParagraphDown", |editor| {
        editor.cancel_autocomplete();
        editor.select_paragraph_down();
    }),
    ("tui.editor.selectDocStart", |editor| {
        editor.cancel_autocomplete();
        editor.select_doc_start();
    }),
    ("tui.editor.selectDocEnd", |editor| {
        editor.cancel_autocomplete();
        editor.select_doc_end();
    }),
];

/// The edit and motion keys, dispatched after the completion menu had its turn. Doc/paragraph
/// jumps have no TS counterpart, so an open completion dropdown has no anchor semantics for them:
/// the dropdown closes instead of staying stale at the old token.
const EDIT_KEYS: &[EditorKey] = &[
    ("tui.editor.deleteToLineEnd", Editor::delete_to_end_of_line),
    (
        "tui.editor.deleteToLineStart",
        Editor::delete_to_start_of_line,
    ),
    (
        "tui.editor.deleteWordBackward",
        Editor::delete_word_backwards,
    ),
    ("tui.editor.deleteWordForward", Editor::delete_word_forward),
    ("tui.editor.deleteCharBackward", Editor::handle_backspace),
    (
        "tui.editor.deleteCharForward",
        Editor::handle_forward_delete,
    ),
    ("tui.editor.yank", Editor::yank),
    ("tui.editor.yankPop", Editor::yank_pop),
    ("tui.editor.cursorLineStart", |editor| {
        editor.clear_selection();
        editor.move_to_line_start();
    }),
    ("tui.editor.cursorLineEnd", |editor| {
        editor.clear_selection();
        editor.move_to_line_end();
    }),
    ("tui.editor.cursorDocStart", |editor| {
        editor.cancel_autocomplete();
        editor.clear_selection();
        editor.move_to_doc_start();
    }),
    ("tui.editor.cursorDocEnd", |editor| {
        editor.cancel_autocomplete();
        editor.clear_selection();
        editor.move_to_doc_end();
    }),
    ("tui.editor.cursorParagraphUp", |editor| {
        editor.cancel_autocomplete();
        editor.clear_selection();
        editor.move_paragraph_backward();
    }),
    ("tui.editor.cursorParagraphDown", |editor| {
        editor.cancel_autocomplete();
        editor.clear_selection();
        editor.move_paragraph_forward();
    }),
    ("tui.editor.cursorWordLeft", |editor| {
        editor.clear_selection();
        editor.move_word_backwards();
    }),
    ("tui.editor.cursorWordRight", |editor| {
        editor.clear_selection();
        editor.move_word_forwards();
    }),
    ("tui.input.newLine", Editor::add_new_line),
    // A trailing backslash continues the line instead of submitting: the backslash becomes a
    // newline.
    ("tui.input.submit", |editor| {
        if editor.disable_submit {
            return;
        }
        let current_line = &editor.lines[editor.cursor_line];
        if editor.cursor_col > 0 && char_at(current_line, editor.cursor_col - 1) == Some('\\') {
            editor.handle_backspace();
            editor.add_new_line();
            return;
        }
        editor.submit_value();
    }),
    ("tui.editor.cursorUp", |editor| {
        editor.clear_selection();
        if editor.is_editor_empty()
            || (editor.is_history_navigation_active() && editor.is_on_first_visual_line())
        {
            editor.navigate_history(-1);
        } else if editor.is_on_first_visual_line() {
            editor.move_to_line_start();
        } else {
            editor.move_cursor(-1, 0);
        }
    }),
    ("tui.editor.cursorDown", |editor| {
        editor.clear_selection();
        if editor.is_history_navigation_active() && editor.is_on_last_visual_line() {
            editor.navigate_history(1);
        } else if editor.is_on_last_visual_line() {
            editor.move_to_line_end();
        } else {
            editor.move_cursor(1, 0);
        }
    }),
    ("tui.editor.cursorRight", |editor| {
        editor.clear_selection();
        editor.move_cursor(0, 1);
    }),
    ("tui.editor.cursorLeft", |editor| {
        editor.clear_selection();
        editor.move_cursor(0, -1);
    }),
    ("tui.editor.pageUp", |editor| {
        editor.clear_selection();
        editor.page_scroll(-1);
    }),
    ("tui.editor.pageDown", |editor| {
        editor.clear_selection();
        editor.page_scroll(1);
    }),
    ("tui.editor.jumpForward", |editor| {
        editor.clear_selection();
        editor.jump_mode = Some(JumpDirection::Forward);
    }),
    ("tui.editor.jumpBackward", |editor| {
        editor.clear_selection();
        editor.jump_mode = Some(JumpDirection::Backward);
    }),
];

/// The literal key a binding also answers to whatever the keymap says: shift+backspace and
/// shift+delete always delete a character.
fn literal_alias(binding: &str) -> Option<&'static str> {
    match binding {
        "tui.editor.deleteCharBackward" => Some("shift+backspace"),
        "tui.editor.deleteCharForward" => Some("shift+delete"),
        _ => None,
    }
}

impl Editor {
    /// Handle one key event (already decoded to a TS-style key id, e.g.
    /// "ctrl+k", or a literal character for printable input).
    pub fn handle_input(&mut self, input: &str) {
        if let Some(direction) = self.jump_mode.take() {
            if self.kb_matches(input, "tui.editor.jumpForward")
                || self.kb_matches(input, "tui.editor.jumpBackward")
            {
                return;
            }
            if let Some(printable) = decode_printable(input) {
                self.jump_to_char(&printable, direction == JumpDirection::Forward);
                return;
            }
        }
        if self.run_key(SELECTION_KEYS, input) || self.handle_autocomplete_key(input) {
            return;
        }
        if self.kb_matches(input, "tui.input.tab") && self.autocomplete.is_none() {
            self.handle_tab_completion();
            return;
        }
        if self.run_key(EDIT_KEYS, input) {
            return;
        }
        if input == "shift+space" {
            self.insert_character(" ");
            return;
        }
        if let Some(printable) = decode_printable(input) {
            self.insert_character(&printable);
        }
    }

    fn kb_matches(&self, input: &str, binding: &str) -> bool {
        self.keybindings.matches(input, binding)
    }

    /// Run the first key in `keys` whose binding matches `input`; false when none does.
    fn run_key(&mut self, keys: &[EditorKey], input: &str) -> bool {
        let Some((_, action)) = keys.iter().find(|(binding, _)| {
            self.kb_matches(input, binding) || literal_alias(binding) == Some(input)
        }) else {
            return false;
        };
        action(self);
        true
    }

    /// The open completion menu's keys: cancel, move, and accept the selected item. Returns true
    /// when the key was consumed; an accept that completes an exactly typed slash command to
    /// itself falls through, so Enter submits.
    fn handle_autocomplete_key(&mut self, input: &str) -> bool {
        // A confirm must read a menu that answers the current text: a burst
        // that outran the dropdown (`/` opened it, the rest of `/exit` parked a
        // refresh) resolves the parked refresh first, so Enter or Tab never
        // splices the stale menu's selection over the typed text.
        let confirm =
            self.kb_matches(input, "tui.input.tab") || self.kb_matches(input, "tui.select.confirm");
        if self.autocomplete.is_some() && self.pending_autocomplete.is_some() && confirm {
            self.materialize_autocomplete();
        }
        let Some(state) = self.autocomplete.as_mut() else {
            return false;
        };
        if self.keybindings.matches(input, "tui.select.cancel") {
            self.cancel_autocomplete();
            return true;
        }
        if self.keybindings.matches(input, "tui.select.up") {
            state.move_up();
            return true;
        }
        if self.keybindings.matches(input, "tui.select.down") {
            state.move_down();
            return true;
        }
        if !confirm {
            return false;
        }
        let Some(item) = state.selected_item() else {
            return false;
        };
        let is_typed_exact = self.is_slash_name_completion_at_prompt_start();
        self.push_undo_snapshot();
        self.last_action = None;
        // A completion rewrites the buffer at the cursor: the selection collapses with it.
        self.selection_anchor = None;
        let (cl, cc) = (self.cursor_line, self.cursor_col);
        let prefix = self
            .autocomplete
            .as_ref()
            .map(|s| s.prefix.clone())
            .unwrap_or_default();
        let result = self.apply_completion(&item, &prefix);
        let completed_noop =
            result.lines == self.lines && result.cursor_line == cl && result.cursor_col == cc;
        self.lines = result.lines;
        self.cursor_line = result.cursor_line;
        self.set_cursor_col(result.cursor_col);
        self.cancel_autocomplete();
        self.request_autocomplete_in_symbol_token();
        if !is_typed_exact || !completed_noop {
            self.emit(EditorEvent::Changed(self.get_text()));
            return true;
        }
        // Exact slash command typed: fall through so Enter submits.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed() -> Editor {
        Editor::new()
    }

    #[test]
    fn jump_mode() {
        let mut e = ed();
        e.set_text("hello world");
        e.move_to_line_start();
        e.handle_input("ctrl+]");
        e.handle_input("w");
        assert_eq!(e.get_cursor(), (0, 6));
    }

    /// A keystroke burst (typed command + Enter in one batch, the tmux
    /// send-keys pattern) submits as typed: the request is parked, so the
    /// dropdown never opens between the keys.
    #[test]
    fn typed_slash_burst_submits_as_typed() {
        let mut e = ed();
        for key in ["/", "g", "o", "a", "l"] {
            e.handle_input(key);
        }
        assert!(!e.is_showing_autocomplete(), "dropdown parks its request");
        e.handle_input("enter");
        let events = e.take_events();
        let submitted = events
            .iter()
            .filter_map(|event| match event {
                EditorEvent::Submitted(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(submitted, vec!["/goal"]);
        assert_eq!(e.get_text(), "");
    }

    /// The TS async-suggestion behavior for character-by-character typing
    /// with pauses.
    #[test]
    fn materialized_dropdown_enter_completes_into_args() {
        let mut e = ed();
        for key in ["/", "g", "o", "a", "l"] {
            e.handle_input(key);
        }
        e.materialize_autocomplete();
        assert!(
            e.is_showing_autocomplete(),
            "dropdown opens after the batch"
        );
        e.handle_input("enter");
        let events = e.take_events();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EditorEvent::Submitted(_)))
        );
        assert_eq!(e.get_text(), "/goal ");
    }

    /// A typed ZWJ family sequence lands in the buffer byte-exact: one key
    /// event per cluster char, each char inserted at the cursor (the
    /// 2026-09-22 harness reproduction of the audit crash class).
    #[test]
    fn typed_zwj_family_lands_intact() {
        let mut e = ed();
        for ch in "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}x".chars() {
            e.handle_input(&ch.to_string());
        }
        eprintln!("text: {:?}", e.get_text());
        eprintln!("lines: {:?}", e.lines);
        assert_eq!(
            e.get_text(),
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}x"
        );
    }
}
