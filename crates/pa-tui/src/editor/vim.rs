//! Vim editing mode for the prompt editor (Claude Code's `editorMode:
//! "vim"`, toggled by `/vim`): INSERT mode types as usual and Escape enters
//! NORMAL mode, where `h j k l w b e 0 $` move, `x`, `d`/`c`/`y` with a
//! motion (and the doubled `dd`/`cc`/`yy`), `D`, `C`, `Y`, `p`/`P` edit,
//! `u` undoes, `i a A I o O` return to INSERT, and a count prefixes any of
//! them. Keys NORMAL mode does not own (Enter, arrows, control chords)
//! keep their ordinary editor and app meaning, and Escape in NORMAL mode
//! reaches the app's own Escape handling.

use super::text_utils::decode_printable;
use super::{Editor, EditorEvent};

/// The vim mode the editor is in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VimMode {
    #[default]
    Insert,
    Normal,
}

impl VimMode {
    /// The mode's name in the status line payload (`vim.mode`).
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            VimMode::Insert => "INSERT",
            VimMode::Normal => "NORMAL",
        }
    }

    /// The indicator under the prompt: Claude Code shows `-- INSERT --`
    /// and nothing in NORMAL mode (the block cursor marks it).
    #[must_use]
    pub fn indicator(self) -> Option<&'static str> {
        match self {
            VimMode::Insert => Some("-- INSERT --"),
            VimMode::Normal => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Delete,
    Change,
    Yank,
}

impl Operator {
    fn from_key(key: char) -> Option<Self> {
        match key {
            'd' => Some(Operator::Delete),
            'c' => Some(Operator::Change),
            'y' => Some(Operator::Yank),
            _ => None,
        }
    }

    fn key(self) -> char {
        match self {
            Operator::Delete => 'd',
            Operator::Change => 'c',
            Operator::Yank => 'y',
        }
    }
}

/// The unnamed register: what the last delete or yank took.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Register {
    text: String,
    linewise: bool,
}

/// Vim state: the mode, a pending count, a pending operator, the register.
#[derive(Debug, Default)]
pub(crate) struct VimState {
    mode: VimMode,
    count: Option<usize>,
    operator: Option<(Operator, usize)>,
    register: Register,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Blank,
    Word,
    Punct,
}

fn class(ch: char) -> CharClass {
    if ch.is_whitespace() {
        CharClass::Blank
    } else if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// The start of the next word after `at` (`w`).
fn next_word_start(chars: &[char], at: usize) -> usize {
    let mut index = at;
    if index >= chars.len() {
        return chars.len();
    }
    let start_class = class(chars[index]);
    if start_class != CharClass::Blank {
        while index < chars.len() && class(chars[index]) == start_class {
            index += 1;
        }
    }
    while index < chars.len() && class(chars[index]) == CharClass::Blank {
        index += 1;
    }
    index
}

/// The last character of the word ending after `at` (`e`).
fn word_end(chars: &[char], at: usize) -> usize {
    if chars.is_empty() {
        return 0;
    }
    let mut index = (at + 1).min(chars.len() - 1);
    while index + 1 < chars.len() && class(chars[index]) == CharClass::Blank {
        index += 1;
    }
    let word_class = class(chars[index]);
    while index + 1 < chars.len() && class(chars[index + 1]) == word_class {
        index += 1;
    }
    index
}

/// The start of the word before `at` (`b`).
fn previous_word_start(chars: &[char], at: usize) -> usize {
    if at == 0 {
        return 0;
    }
    let mut index = at - 1;
    while index > 0 && class(chars[index]) == CharClass::Blank {
        index -= 1;
    }
    let word_class = class(chars[index]);
    while index > 0 && class(chars[index - 1]) == word_class {
        index -= 1;
    }
    index
}

/// The bounds of the line holding `at`: its first index and the index of
/// its newline (or the text's end).
fn line_bounds(chars: &[char], at: usize) -> (usize, usize) {
    let start = chars[..at.min(chars.len())]
        .iter()
        .rposition(|ch| *ch == '\n')
        .map_or(0, |newline| newline + 1);
    let end = chars[at.min(chars.len())..]
        .iter()
        .position(|ch| *ch == '\n')
        .map_or(chars.len(), |offset| at + offset);
    (start, end)
}

fn first_non_blank(line: &str) -> usize {
    line.chars().position(|ch| !ch.is_whitespace()).unwrap_or(0)
}

impl Editor {
    /// Turn vim mode on (it starts in INSERT, like Claude Code) or off.
    pub fn set_vim_enabled(&mut self, enabled: bool) {
        match (enabled, self.vim.is_some()) {
            (true, false) => self.vim = Some(VimState::default()),
            (false, true) => self.vim = None,
            (true, true) | (false, false) => {}
        }
    }

    #[must_use]
    pub fn vim_enabled(&self) -> bool {
        self.vim.is_some()
    }

    /// The vim mode, or `None` with vim off.
    #[must_use]
    pub fn vim_mode(&self) -> Option<VimMode> {
        self.vim.as_ref().map(|vim| vim.mode)
    }

    /// Offer one key to vim mode first; `true` when vim consumed it.
    pub fn handle_vim_key(&mut self, id: &str) -> bool {
        let Some(mode) = self.vim_mode() else {
            return false;
        };
        match mode {
            VimMode::Insert => {
                // An open completion menu keeps its own Escape.
                if id != "escape" || self.is_showing_autocomplete() {
                    return false;
                }
                self.cancel_autocomplete();
                self.set_vim_mode(VimMode::Normal);
                let col = self.cursor_col.saturating_sub(1);
                self.set_cursor_col(col);
                true
            }
            VimMode::Normal => {
                if id == "escape" {
                    // Escape cancels a pending count/operator; a bare one
                    // is the app's (interrupt, clear, double-Esc).
                    return self.vim.as_mut().is_some_and(|vim| {
                        let counted = vim.count.take().is_some();
                        let operator = vim.operator.take().is_some();
                        counted || operator
                    });
                }
                if id == "backspace" {
                    self.vim_normal_key('h');
                    return true;
                }
                let Some(text) = decode_printable(id) else {
                    return false;
                };
                let mut chars = text.chars();
                match (chars.next(), chars.next()) {
                    (Some(' '), None) => self.vim_normal_key('l'),
                    (Some(key), None) => self.vim_normal_key(key),
                    _ => return false,
                }
                true
            }
        }
    }

    fn set_vim_mode(&mut self, mode: VimMode) {
        if let Some(vim) = self.vim.as_mut() {
            vim.mode = mode;
            vim.count = None;
            vim.operator = None;
        }
    }

    /// The whole text as characters and the cursor's index in it.
    fn vim_flat(&self) -> (Vec<char>, usize) {
        let mut cursor = 0;
        for line in &self.lines[..self.cursor_line] {
            cursor += line.chars().count() + 1;
        }
        let line_len = self.lines[self.cursor_line].chars().count();
        (
            self.get_text().chars().collect(),
            cursor + self.cursor_col.min(line_len),
        )
    }

    /// Move the cursor to a flat index (text unchanged).
    fn vim_move_to(&mut self, chars: &[char], at: usize) {
        let at = at.min(chars.len());
        let line = chars[..at].iter().filter(|ch| **ch == '\n').count();
        let (start, _) = line_bounds(chars, at);
        self.cursor_line = line.min(self.lines.len() - 1);
        self.set_cursor_col(at - start);
    }

    /// Replace the text and put the cursor at a flat index.
    fn vim_replace(&mut self, chars: &[char], at: usize) {
        let text: String = chars.iter().collect();
        self.lines = text.split('\n').map(str::to_string).collect();
        self.selection_anchor = None;
        self.vim_move_to(chars, at);
        self.emit(EditorEvent::Changed(self.get_text()));
    }

    /// NORMAL mode keeps the cursor on a character (never past the last one).
    fn vim_clamp_cursor(&mut self) {
        let len = self.lines[self.cursor_line].chars().count();
        if self.cursor_col >= len {
            self.set_cursor_col(len.saturating_sub(1));
        }
    }

    fn vim_enter_insert(&mut self) {
        self.set_vim_mode(VimMode::Insert);
    }

    fn vim_normal_key(&mut self, key: char) {
        let Some(vim) = self.vim.as_mut() else {
            return;
        };
        if let Some(digit) = key.to_digit(10) {
            if digit != 0 || vim.count.is_some() {
                vim.count = Some(vim.count.unwrap_or(0) * 10 + digit as usize);
                return;
            }
        }
        let count = vim.count.take().unwrap_or(1);
        if let Some((operator, operator_count)) = vim.operator.take() {
            self.vim_operator(operator, count * operator_count, key);
            return;
        }
        match key {
            'h' | 'l' | 'j' | 'k' | 'w' | 'b' | 'e' | '0' | '$' => self.vim_motion(key, count),
            'x' => self.vim_delete_chars(count),
            'd' | 'c' | 'y' => {
                if let (Some(vim), Some(operator)) = (self.vim.as_mut(), Operator::from_key(key)) {
                    vim.operator = Some((operator, count));
                }
            }
            'D' => self.vim_operator(Operator::Delete, 1, '$'),
            'C' => self.vim_operator(Operator::Change, 1, '$'),
            'Y' => self.vim_lines(Operator::Yank, count),
            'p' => self.vim_paste(count, /*after*/ true),
            'P' => self.vim_paste(count, /*after*/ false),
            'u' => {
                for _ in 0..count {
                    self.undo();
                }
                self.vim_clamp_cursor();
            }
            'i' => self.vim_enter_insert(),
            'a' => {
                let len = self.lines[self.cursor_line].chars().count();
                self.set_cursor_col((self.cursor_col + 1).min(len));
                self.vim_enter_insert();
            }
            'A' => {
                self.move_to_line_end();
                self.vim_enter_insert();
            }
            'I' => {
                let col = first_non_blank(&self.lines[self.cursor_line]);
                self.set_cursor_col(col);
                self.vim_enter_insert();
            }
            'o' | 'O' => self.vim_open_line(key == 'o'),
            // Every other printable key is a NORMAL-mode no-op: it never types.
            _ => {}
        }
    }

    /// A cursor motion repeated `count` times.
    fn vim_motion(&mut self, key: char, count: usize) {
        match key {
            'j' => {
                self.cursor_line = (self.cursor_line + count).min(self.lines.len() - 1);
                self.vim_clamp_cursor();
                return;
            }
            'k' => {
                self.cursor_line = self.cursor_line.saturating_sub(count);
                self.vim_clamp_cursor();
                return;
            }
            _ => {}
        }
        let (chars, cursor) = self.vim_flat();
        let (start, end) = line_bounds(&chars, cursor);
        let target = match key {
            'h' => cursor.saturating_sub(count).max(start),
            'l' => (cursor + count).min(end.saturating_sub(1).max(start)),
            '0' => start,
            '$' => end.saturating_sub(1).max(start),
            'w' => (0..count).fold(cursor, |at, _| next_word_start(&chars, at)),
            'b' => (0..count).fold(cursor, |at, _| previous_word_start(&chars, at)),
            'e' => (0..count).fold(cursor, |at, _| word_end(&chars, at)),
            _ => cursor,
        };
        self.last_action = None;
        self.vim_move_to(&chars, target);
        self.vim_clamp_cursor();
    }

    /// `x`: delete up to `count` characters under and after the cursor.
    fn vim_delete_chars(&mut self, count: usize) {
        let (mut chars, cursor) = self.vim_flat();
        let (_, end) = line_bounds(&chars, cursor);
        let stop = (cursor + count).min(end);
        if stop <= cursor {
            return;
        }
        self.push_undo_snapshot();
        let taken: String = chars.drain(cursor..stop).collect();
        self.vim_store(taken, /*linewise*/ false);
        self.vim_replace(&chars, cursor);
        self.vim_clamp_cursor();
    }

    /// An operator applied with the key that followed it: the same key
    /// doubled works on whole lines, a motion on the range it covers.
    fn vim_operator(&mut self, operator: Operator, count: usize, key: char) {
        if key == operator.key() {
            self.vim_lines(operator, count);
            return;
        }
        let (chars, cursor) = self.vim_flat();
        let (start, end) = line_bounds(&chars, cursor);
        let range = match key {
            'w' if operator == Operator::Change => {
                // `cw` changes to the end of the word, like `ce`.
                let last = (0..count).fold(cursor, |at, _| word_end(&chars, at));
                let last = if class(chars.get(cursor).copied().unwrap_or(' ')) == CharClass::Blank {
                    cursor
                } else {
                    last.max(cursor)
                };
                cursor..(last + 1).min(chars.len())
            }
            'w' => {
                let target = (0..count).fold(cursor, |at, _| next_word_start(&chars, at));
                // `dw` on a line's last word stops at the line's end.
                cursor..target.min(end.max(cursor))
            }
            'e' => {
                let last = (0..count).fold(cursor, |at, _| word_end(&chars, at));
                cursor..(last + 1).min(chars.len())
            }
            'b' => (0..count).fold(cursor, |at, _| previous_word_start(&chars, at))..cursor,
            'h' => cursor.saturating_sub(count).max(start)..cursor,
            'l' => cursor..(cursor + count).min(end),
            '0' => start..cursor,
            '$' => cursor..end,
            _ => return,
        };
        if range.is_empty() && operator != Operator::Change {
            return;
        }
        let taken: String = chars[range.clone()].iter().collect();
        self.vim_store(taken, /*linewise*/ false);
        match operator {
            Operator::Yank => {
                self.vim_move_to(&chars, range.start);
                self.vim_clamp_cursor();
            }
            Operator::Delete | Operator::Change => {
                self.push_undo_snapshot();
                let mut chars = chars;
                chars.drain(range.clone());
                self.vim_replace(&chars, range.start);
                if operator == Operator::Change {
                    self.vim_enter_insert();
                } else {
                    self.vim_clamp_cursor();
                }
            }
        }
    }

    /// `dd`/`cc`/`yy` (and `Y`): `count` whole lines from the cursor's.
    fn vim_lines(&mut self, operator: Operator, count: usize) {
        let first = self.cursor_line;
        let last = (first + count - 1).min(self.lines.len() - 1);
        self.vim_store(self.lines[first..=last].join("\n"), /*linewise*/ true);
        match operator {
            Operator::Yank => {}
            Operator::Delete => {
                self.push_undo_snapshot();
                self.lines.drain(first..=last);
                if self.lines.is_empty() {
                    self.lines.push(String::new());
                }
                self.cursor_line = first.min(self.lines.len() - 1);
                let col = first_non_blank(&self.lines[self.cursor_line]);
                self.set_cursor_col(col);
                self.selection_anchor = None;
                self.emit(EditorEvent::Changed(self.get_text()));
            }
            Operator::Change => {
                self.push_undo_snapshot();
                self.lines
                    .splice(first..=last, std::iter::once(String::new()));
                self.cursor_line = first;
                self.set_cursor_col(0);
                self.selection_anchor = None;
                self.emit(EditorEvent::Changed(self.get_text()));
                self.vim_enter_insert();
            }
        }
    }

    /// `p`/`P`: the register after or before the cursor, `count` times.
    fn vim_paste(&mut self, count: usize, after: bool) {
        let Some(register) = self.vim.as_ref().map(|vim| vim.register.clone()) else {
            return;
        };
        if register.text.is_empty() && !register.linewise {
            return;
        }
        self.push_undo_snapshot();
        if register.linewise {
            let at = if after {
                self.cursor_line + 1
            } else {
                self.cursor_line
            };
            let pasted: Vec<String> = (0..count)
                .flat_map(|_| register.text.split('\n').map(str::to_string))
                .collect();
            self.lines.splice(at..at, pasted);
            self.cursor_line = at;
            let col = first_non_blank(&self.lines[at]);
            self.set_cursor_col(col);
            self.selection_anchor = None;
            self.emit(EditorEvent::Changed(self.get_text()));
            return;
        }
        let (mut chars, cursor) = self.vim_flat();
        let (_, end) = line_bounds(&chars, cursor);
        let at = if after { (cursor + 1).min(end) } else { cursor };
        let text: Vec<char> = register.text.repeat(count).chars().collect();
        let landed = at + text.len().saturating_sub(1);
        chars.splice(at..at, text);
        self.vim_replace(&chars, landed);
    }

    /// `o`/`O`: a new line below or above, in INSERT mode.
    fn vim_open_line(&mut self, below: bool) {
        self.push_undo_snapshot();
        let at = if below {
            self.cursor_line + 1
        } else {
            self.cursor_line
        };
        self.lines.insert(at, String::new());
        self.cursor_line = at;
        self.set_cursor_col(0);
        self.selection_anchor = None;
        self.emit(EditorEvent::Changed(self.get_text()));
        self.vim_enter_insert();
    }

    fn vim_store(&mut self, text: String, linewise: bool) {
        if let Some(vim) = self.vim.as_mut() {
            vim.register = Register { text, linewise };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> Editor {
        let mut editor = Editor::new();
        editor.clear_autocomplete_provider();
        editor.set_vim_enabled(true);
        editor.set_text(text);
        editor
    }

    /// Feed keys: `<esc>` is Escape, every other char is its own key.
    fn keys(editor: &mut Editor, script: &str) {
        let mut rest = script;
        while !rest.is_empty() {
            if let Some(tail) = rest.strip_prefix("<esc>") {
                if !editor.handle_vim_key("escape") {
                    editor.handle_input("escape");
                }
                rest = tail;
                continue;
            }
            let ch = rest.chars().next().unwrap();
            let id = if ch == ' ' {
                "space".to_string()
            } else if ch.is_ascii_uppercase() {
                format!("shift+{}", ch.to_ascii_lowercase())
            } else {
                ch.to_string()
            };
            if !editor.handle_vim_key(&id) {
                editor.handle_input(&id);
            }
            rest = &rest[ch.len_utf8()..];
        }
    }

    fn state(editor: &Editor) -> (String, usize, usize, Option<VimMode>) {
        (
            editor.get_text(),
            editor.cursor_line,
            editor.cursor_col,
            editor.vim_mode(),
        )
    }

    #[test]
    fn escape_enters_normal_and_insert_keys_return() {
        let mut e = editor("hello");
        assert_eq!(e.vim_mode(), Some(VimMode::Insert));
        keys(&mut e, "<esc>");
        assert_eq!(state(&e), ("hello".into(), 0, 4, Some(VimMode::Normal)));
        // NORMAL mode never types.
        keys(&mut e, "zq");
        assert_eq!(e.get_text(), "hello");
        keys(&mut e, "0iX<esc>");
        assert_eq!(state(&e), ("Xhello".into(), 0, 0, Some(VimMode::Normal)));
        keys(&mut e, "AY<esc>");
        assert_eq!(state(&e), ("XhelloY".into(), 0, 6, Some(VimMode::Normal)));
        keys(&mut e, "0aZ<esc>");
        assert_eq!(state(&e), ("XZhelloY".into(), 0, 1, Some(VimMode::Normal)));
        keys(&mut e, "oline<esc>Oup<esc>");
        assert_eq!(e.get_text(), "XZhelloY\nup\nline");
        keys(&mut e, "I  <esc>");
        assert_eq!(e.get_text(), "XZhelloY\n  up\nline");
    }

    #[test]
    fn motions_take_counts() {
        let mut e = editor("one two three four\nsecond line");
        keys(&mut e, "<esc>k0w");
        assert_eq!((e.cursor_line, e.cursor_col), (0, 4));
        keys(&mut e, "2w");
        assert_eq!((e.cursor_line, e.cursor_col), (0, 14));
        keys(&mut e, "e");
        assert_eq!((e.cursor_line, e.cursor_col), (0, 17));
        keys(&mut e, "b");
        assert_eq!((e.cursor_line, e.cursor_col), (0, 14));
        keys(&mut e, "$");
        assert_eq!((e.cursor_line, e.cursor_col), (0, 17));
        keys(&mut e, "j");
        assert_eq!((e.cursor_line, e.cursor_col), (1, 10));
        keys(&mut e, "0llk3h");
        assert_eq!((e.cursor_line, e.cursor_col), (0, 0));
    }

    #[test]
    fn deletes_changes_yanks_and_pastes() {
        let mut e = editor("alpha beta gamma");
        keys(&mut e, "<esc>0dw");
        assert_eq!(
            state(&e),
            ("beta gamma".into(), 0, 0, Some(VimMode::Normal))
        );
        keys(&mut e, "2x");
        assert_eq!(e.get_text(), "ta gamma");
        keys(&mut e, "cwXY<esc>");
        assert_eq!(state(&e), ("XY gamma".into(), 0, 1, Some(VimMode::Normal)));
        keys(&mut e, "wD");
        assert_eq!(e.get_text(), "XY ");
        keys(&mut e, "u");
        assert_eq!(e.get_text(), "XY gamma");
        // `dw` on the last word stops at the line's end.
        keys(&mut e, "0w2dw");
        assert_eq!(e.get_text(), "XY ");
        keys(&mut e, "0yw$p");
        assert_eq!(e.get_text(), "XY XY ");
    }

    #[test]
    fn linewise_operators_and_paste() {
        let mut e = editor("one\ntwo\nthree");
        keys(&mut e, "<esc>gg");
        keys(&mut e, "kkdd");
        assert_eq!(
            state(&e),
            ("two\nthree".into(), 0, 0, Some(VimMode::Normal))
        );
        keys(&mut e, "p");
        assert_eq!(
            state(&e),
            ("two\none\nthree".into(), 1, 0, Some(VimMode::Normal))
        );
        keys(&mut e, "yyjP");
        assert_eq!(e.get_text(), "two\none\none\nthree");
        keys(&mut e, "2dd");
        assert_eq!(e.get_text(), "two\none");
        keys(&mut e, "ccnew<esc>");
        assert_eq!(e.get_text(), "two\nnew");
        keys(&mut e, "k5dd");
        assert_eq!(state(&e), (String::new(), 0, 0, Some(VimMode::Normal)));
    }

    #[test]
    fn escape_in_normal_mode_reaches_the_app_unless_a_command_is_pending() {
        let mut e = editor("text");
        assert!(e.handle_vim_key("escape"));
        assert!(!e.handle_vim_key("escape"));
        assert!(e.handle_vim_key("d"));
        assert!(e.handle_vim_key("escape"));
        assert!(e.handle_vim_key("x"));
        assert_eq!(e.get_text(), "tex");
        // Enter and control chords stay the editor's and the app's.
        assert!(!e.handle_vim_key("enter"));
        assert!(!e.handle_vim_key("ctrl+c"));
        e.set_vim_enabled(false);
        assert!(!e.handle_vim_key("x"));
    }
}
