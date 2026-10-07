//! The guard's word scanner: the argv the shell builds from (normalized)
//! command text, with each word's span, whether it starts a command, and
//! whether it is a command-substitution interior.

use super::messages;
use super::normalize::fold_ansi_c_span;
use super::pyos::is_space;

/// One shell word: its folded argv value and the span it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellWord {
    pub value: String,
    pub start: usize,
    pub end: usize,
    /// The first word of a fresh (sub)command context.
    pub starts_command: bool,
    /// A command-substitution interior: its span sits inside the enclosing
    /// word, which the scanner appends after the interiors it recursed into.
    pub contained: bool,
}

impl ShellWord {
    /// Whether this word opens a run of its own (a command start that is not
    /// a substitution interior).
    pub(super) fn heads_run(&self) -> bool {
        self.starts_command && !self.contained
    }
}

/// `text[start:end]` with Python's clamping: out-of-range bounds shrink and
/// a reversed range is empty.
pub(super) fn py_slice(text: &[char], start: usize, end: usize) -> &[char] {
    let end = end.min(text.len());
    &text[start.min(end)..end]
}

/// The `)` matching the `(` at `open_index`, or `end - 1`: quotes and
/// escapes are tracked (inside double quotes and backticks only a nested
/// `$(` counts), so a quoted `)` never closes the substitution.
pub(super) fn matching_paren(command: &[char], open_index: usize, end: usize) -> usize {
    let mut depth = 0i64;
    let mut quote: Option<char> = None;
    let mut i = open_index;
    while i < end {
        let ch = command[i];
        match quote {
            None => {
                if ch == '\\' && i + 1 < end {
                    i += 2;
                    continue;
                }
                if matches!(ch, '\'' | '"' | '`') {
                    quote = Some(ch);
                } else if ch == '(' {
                    depth += 1;
                } else if ch == ')' {
                    depth -= 1;
                    if depth == 0 {
                        return i;
                    }
                }
            }
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some(open) => {
                if ch == '\\' && i + 1 < end {
                    i += 2;
                    continue;
                }
                if ch == open {
                    quote = None;
                } else if ch == '$' && command.get(i + 1) == Some(&'(') {
                    depth += 1;
                    i += 1;
                } else if ch == ')' && depth > 1 {
                    depth -= 1;
                }
            }
        }
        i += 1;
    }
    end.saturating_sub(1)
}

/// The backtick closing a substitution, skipping backslash pairs, or `None`.
pub(super) fn backtick_close(command: &[char], start: usize, end: usize) -> Option<usize> {
    let mut i = start;
    while i < end {
        match command[i] {
            '\\' => i += 2,
            '`' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// The scanner's state for one region.
struct Region<'a> {
    command: &'a [char],
    end: usize,
    depth: usize,
    value: String,
    word_start: Option<usize>,
    word_starts_command: bool,
    first_word_pending: bool,
}

struct Scanner<'a> {
    command: &'a [char],
    words: Vec<ShellWord>,
}

impl Scanner<'_> {
    fn scan_region(
        &mut self,
        start: usize,
        end: usize,
        starts_command: bool,
        depth: usize,
    ) -> Result<(), String> {
        if depth > messages::MAX_SUBSTITUTION_NESTING {
            return Err(messages::nesting());
        }
        let command = self.command;
        let mut region = Region {
            command,
            end,
            depth,
            value: String::new(),
            word_start: None,
            word_starts_command: false,
            first_word_pending: starts_command,
        };
        let mut i = start;
        while i < end {
            let ch = command[i];
            if matches!(ch, ' ' | '\t' | '\r') {
                self.flush(&mut region, i, false);
                i += 1;
                continue;
            }
            if "\n;|&()<>".contains(ch) {
                self.flush(&mut region, i, true);
                i += 1;
                continue;
            }
            if ch == '#' && region.word_start.is_none() {
                while i < end && command[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            if region.word_start.is_none() {
                region.word_start = Some(i);
                region.word_starts_command = region.first_word_pending;
                region.first_word_pending = false;
            }
            if ch == '\\' && i + 1 < end {
                region.value.push(command[i + 1]);
                i += 2;
                continue;
            }
            if ch == '\'' {
                let mut j = i + 1;
                while j < end && command[j] != '\'' {
                    j += 1;
                }
                region.value.extend(py_slice(command, i + 1, j));
                i = j + 1;
                continue;
            }
            if ch == '"' {
                i = self.scan_double_quote(&mut region, i + 1)?;
                continue;
            }
            if ch == '$' && command.get(i + 1) == Some(&'\'') {
                let (folded, after) = fold_ansi_c_span(command, i, end);
                region.value.push_str(&folded);
                i = after;
                continue;
            }
            if ch == '$' && command.get(i + 1) == Some(&'"') {
                i = self.scan_double_quote(&mut region, i + 2)?;
                continue;
            }
            if ch == '$' && command.get(i + 1) == Some(&'(') {
                let close = matching_paren(command, i + 1, end);
                self.scan_contained(i + 2, close, depth)?;
                region.value.extend(py_slice(command, i + 1, close + 1));
                i = close + 1;
                continue;
            }
            if ch == '`' {
                let close = backtick_close(command, i + 1, end).unwrap_or(end - 1);
                self.scan_contained(i + 1, close, depth)?;
                region.value.extend(py_slice(command, i + 1, close + 1));
                i = close + 1;
                continue;
            }
            region.value.push(ch);
            i += 1;
        }
        self.flush(&mut region, i, false);
        Ok(())
    }

    /// A double-quoted span from just after its opening quote: escapes fold,
    /// substitution interiors are scanned. Returns the index after the span.
    fn scan_double_quote(&mut self, region: &mut Region<'_>, from: usize) -> Result<usize, String> {
        let command = region.command;
        let end = region.end;
        let mut j = from;
        while j < end {
            let inner = command[j];
            if inner == '\\' && j + 1 < end {
                region.value.push(command[j + 1]);
                j += 2;
                continue;
            }
            if inner == '"' {
                j += 1;
                break;
            }
            if inner == '$' && command.get(j + 1) == Some(&'(') {
                let close = matching_paren(command, j + 1, end);
                self.scan_contained(j + 2, close, region.depth)?;
                region.value.extend(py_slice(command, j + 1, close + 1));
                j = close + 1;
                continue;
            }
            if inner == '`' {
                let close = backtick_close(command, j + 1, end).unwrap_or(end - 1);
                self.scan_contained(j + 1, close, region.depth)?;
                region.value.extend(py_slice(command, j + 1, close + 1));
                j = close + 1;
                continue;
            }
            region.value.push(inner);
            j += 1;
        }
        Ok(j)
    }

    /// Scan a substitution interior and mark every word it produced.
    fn scan_contained(&mut self, start: usize, end: usize, depth: usize) -> Result<(), String> {
        let mark_from = self.words.len();
        self.scan_region(start, end, true, depth + 1)?;
        for word in &mut self.words[mark_from..] {
            word.contained = true;
        }
        Ok(())
    }

    fn flush(&mut self, region: &mut Region<'_>, at: usize, starts_next_command: bool) {
        match region.word_start.take() {
            Some(start) => {
                self.words.push(ShellWord {
                    value: std::mem::take(&mut region.value),
                    start,
                    end: at,
                    starts_command: region.word_starts_command,
                    contained: false,
                });
                region.first_word_pending = starts_next_command;
            }
            None => region.first_word_pending = region.first_word_pending || starts_next_command,
        }
    }
}

/// Split `command` into shell words the way the shell builds argv: quotes
/// and escapes fold into values, comments are skipped, and substitution
/// interiors are scanned as live commands (their text stays in the
/// enclosing word, which therefore reads as unresolvable).
///
/// # Errors
///
/// The nesting refusal past the substitution depth bound.
pub(super) fn scan_shell_words(command: &[char]) -> Result<Vec<ShellWord>, String> {
    let mut scanner = Scanner {
        command,
        words: Vec::new(),
    };
    scanner.scan_region(0, command.len(), true, 0)?;
    Ok(scanner.words)
}

/// The values of the word at `index` and its followers, up to the next
/// command boundary (substitution interiors are looked through).
pub(super) fn run_tokens_from(words: &[ShellWord], index: usize) -> Vec<String> {
    let mut tokens = vec![words[index].value.clone()];
    tokens.extend(run_followers(words, index).map(|follower| follower.value.clone()));
    tokens
}

/// The words after `index` in its run: up to the next command start that is
/// not a substitution interior, with interiors skipped.
pub(super) fn run_followers(words: &[ShellWord], index: usize) -> impl Iterator<Item = &ShellWord> {
    words[index + 1..]
        .iter()
        .take_while(|word| !word.heads_run())
        .filter(|word| !word.starts_command)
}

/// Spans of `$(...)` and backtick substitutions, quote-aware (they still run
/// inside double quotes).
pub(super) fn substitution_spans(command: &[char]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let n = command.len();
    let mut quote: Option<char> = None;
    let mut i = 0;
    let span_at = |i: usize| -> Option<usize> {
        if command[i] == '$' && command.get(i + 1) == Some(&'(') {
            Some(matching_paren(command, i + 1, n))
        } else if command[i] == '`' {
            Some(backtick_close(command, i + 1, n).unwrap_or(n - 1))
        } else {
            None
        }
    };
    while i < n {
        let ch = command[i];
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some(_) => {
                if ch == '\\' {
                    i += 1;
                } else if ch == '"' {
                    quote = None;
                } else if let Some(close) = span_at(i) {
                    spans.push((i, close));
                    i = close;
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                } else if let Some(close) = span_at(i) {
                    spans.push((i, close));
                    i = close;
                }
            }
        }
        i += 1;
    }
    spans
}

/// The shell word ending at `end` (trailing whitespace ignored), or `None`;
/// with `skip_options` option words are stepped over backward.
pub(super) fn word_before(command: &[char], end: usize, skip_options: bool) -> Option<String> {
    let mut j = end;
    loop {
        while j > 0 && is_space(command[j - 1]) {
            j -= 1;
        }
        let mut k = j;
        while k > 0 && !is_space(command[k - 1]) && !";&|<>(){}".contains(command[k - 1]) {
            k -= 1;
        }
        let word: String = command[k..j].iter().collect();
        if word.is_empty() {
            return None;
        }
        if skip_options && word != "--" && word.starts_with('-') {
            j = k;
            continue;
        }
        return Some(word);
    }
}
