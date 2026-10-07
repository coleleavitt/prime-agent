//! Length-preserving masks of the literal spans, the command segments, and
//! the shell's parenthesis matching.

use std::collections::HashMap;

/// Characters a `#` must follow (or be first) to start a comment.
const COMMENT_BOUNDARY: &str = " \t\n;&|(){}";

/// The characters a backslash escapes inside a double quote (POSIX lists the
/// rest as literal, so `"\q"` keeps its backslash); a newline because a
/// backslash before it is a line continuation.
const DOUBLE_QUOTE_ESCAPES: &str = "$`\"\\\n";

/// Characters that end one command segment and start the next.
const SEGMENT_SEPARATORS: &str = ";|&()\n";

/// Characters that end a word: the boundary a `#` needs to start a comment
/// and where a descriptor must stop (`>&2x` is a file called `2x`).
pub(super) const WORD_BREAKERS: &str = " \t\n;&|(){}<>";

/// Whether [`mask_literals`] blanks double-quoted spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DoubleQuotes {
    /// Blank them: the `~` spelling never expands inside one.
    Masked,
    /// Leave them live: `$HOME` expands inside one.
    Live,
}

/// Blank the spans the shell treats as literal data, keeping every index.
///
/// Single-quoted spans and comments (a `#` at a word boundary) are always
/// blanked; double-quoted spans per `double_quotes`. Outside quotes a
/// backslash blanks its pair (walking pairs keeps a doubled backslash's
/// parity right); inside a live double quote only the escapes the shell
/// honors blank their pair, which hides the `$` of an escaped `$HOME`.
pub(super) fn mask_literals(command: &[char], double_quotes: DoubleQuotes) -> Vec<char> {
    let mut chars = command.to_vec();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        if ch == '\'' || (ch == '"' && double_quotes == DoubleQuotes::Masked) {
            let quote = ch;
            let mut j = i + 1;
            while j < n && chars[j] != quote {
                j += if quote == '"' && chars[j] == '\\' {
                    2
                } else {
                    1
                };
            }
            for slot in &mut chars[(i + 1).min(n)..j.min(n)] {
                *slot = ' ';
            }
            i = j + 1;
        } else if ch == '"' {
            i += 1;
            while i < n && chars[i] != '"' {
                if chars[i] == '\\' {
                    if i + 1 < n && DOUBLE_QUOTE_ESCAPES.contains(chars[i + 1]) {
                        chars[i] = ' ';
                        chars[i + 1] = ' ';
                    }
                    i += 2;
                    continue;
                }
                i += 1;
            }
            i += 1;
        } else if ch == '\\' {
            chars[i] = ' ';
            if i + 1 < n {
                chars[i + 1] = ' ';
            }
            i += 2;
        } else if ch == '#' && (i == 0 || COMMENT_BOUNDARY.contains(chars[i - 1])) {
            while i < n && chars[i] != '\n' {
                chars[i] = ' ';
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    chars
}

/// One command segment of a masked command: `[start, end)` and the
/// character that ended it (`None` for the last one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Segment {
    pub start: usize,
    pub end: usize,
    pub separator: Option<char>,
}

/// Split a masked command on unquoted `;`, `&`, `|`, `(`, `)` and newlines.
///
/// An `&` glued to a `>` is a redirect spelling (`2>&1`, `&>`), not a
/// separator. A closed `$(` expansion is skipped whole: its parentheses are
/// syntax, and the substitution walk reads its interior (or, for `$((`, the
/// arithmetic runs nothing). An expansion that never closes keeps its `(` a
/// separator, so the text after it is read as segments (fail closed).
pub(super) fn command_segments(masked: &[char]) -> Vec<Segment> {
    let mut segments = Vec::new();
    let n = masked.len();
    let mut start = 0;
    let mut index = 0;
    let mut matches: Option<ParenMatches> = None;
    while index < n {
        let ch = masked[index];
        if ch == '$' && masked.get(index + 1) == Some(&'(') {
            let matches = matches.get_or_insert_with(|| paren_matches(masked));
            if let Closer::At(close) = matches.closer(index + 1) {
                index = close + 1;
                continue;
            }
        }
        let redirect_amp = ch == '&'
            && ((index > 0 && masked[index - 1] == '>') || masked.get(index + 1) == Some(&'>'));
        if SEGMENT_SEPARATORS.contains(ch) && !redirect_amp {
            segments.push(Segment {
                start,
                end: index,
                separator: Some(ch),
            });
            start = index + 1;
        }
        index += 1;
    }
    segments.push(Segment {
        start,
        end: n,
        separator: None,
    });
    segments
}

/// The index just past the quote that closes `command[quote_index]`, or
/// `None` when the span never closes. A backslash escapes the next character
/// inside a double quote.
pub(super) fn quoted_span_end(command: &[char], quote_index: usize) -> Option<usize> {
    let quote = command[quote_index];
    let mut index = quote_index + 1;
    while index < command.len() {
        if quote == '"' && command[index] == '\\' {
            index += 2;
            continue;
        }
        if command[index] == quote {
            return Some(index + 1);
        }
        index += 1;
    }
    None
}

/// What closes one `(`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Closer {
    /// The scan never opened a paren at that index.
    Missing,
    /// Opened and never closed.
    Unmatched,
    /// Closed at this index.
    At(usize),
}

/// For every `(` the shell opens, the `)` that closes it.
pub(super) struct ParenMatches(HashMap<usize, Option<usize>>);

impl ParenMatches {
    pub(super) fn closer(&self, open: usize) -> Closer {
        match self.0.get(&open) {
            None => Closer::Missing,
            Some(None) => Closer::Unmatched,
            Some(Some(close)) => Closer::At(*close),
        }
    }

    /// Whether the `$((` at `dollar` is arithmetic rather than a subshell:
    /// bash reads it as arithmetic only when the two closers are adjacent
    /// (`$((env))` runs nothing, `$((env) )` is `$( ( env ) )`). As in the
    /// reference, an inner `(` the scan never opened next to an unmatched
    /// outer one also counts as adjacent.
    pub(super) fn is_arithmetic(&self, dollar: usize) -> bool {
        match (self.closer(dollar + 2), self.closer(dollar + 1)) {
            (Closer::At(inner), Closer::At(outer)) => inner + 1 == outer,
            (Closer::Missing, Closer::Unmatched) => true,
            (Closer::Missing | Closer::Unmatched | Closer::At(_), _) => false,
        }
    }
}

/// Match every `(` in one pass, reading quoting the way the shell does: a
/// substitution's interior starts a fresh quoting context (`$(echo "a)")`
/// closes at the last `)`), and a comment (a `#` starting a word, also right
/// after `$(` or `(`) hides its `)`.
pub(super) fn paren_matches(command: &[char]) -> ParenMatches {
    let mut matches = HashMap::new();
    let mut stack: Vec<(usize, bool)> = Vec::new();
    let mut in_double_quotes = false;
    let mut in_word = false;
    let n = command.len();
    let mut index = 0;
    while index < n {
        let ch = command[index];
        if ch == '\\' {
            in_word = true;
            index += 2;
            continue;
        }
        if ch == '\'' {
            // An unterminated span proves nothing: scan on inside it.
            in_word = true;
            index = quoted_span_end(command, index).unwrap_or(index + 1);
            continue;
        }
        if ch == '"' {
            in_double_quotes = !in_double_quotes;
            in_word = true;
            index += 1;
            continue;
        }
        if !in_double_quotes && ch == '#' && !in_word {
            while index < n && command[index] != '\n' {
                index += 1;
            }
            continue;
        }
        if ch == '$' && command.get(index + 1) == Some(&'(') {
            stack.push((index + 1, in_double_quotes));
            in_double_quotes = false;
            in_word = false;
            index += 2;
            continue;
        }
        if ch == '(' && !in_double_quotes {
            stack.push((index, in_double_quotes));
            in_word = false;
            index += 1;
            continue;
        }
        if ch == ')' && !in_double_quotes {
            if let Some((open, saved)) = stack.pop() {
                matches.insert(open, Some(index));
                in_double_quotes = saved;
                in_word = false;
                index += 1;
                continue;
            }
        }
        in_word = !WORD_BREAKERS.contains(ch);
        index += 1;
    }
    for (open, _) in stack {
        matches.insert(open, None);
    }
    ParenMatches(matches)
}

/// A copy of `command` with the given segments blanked, keeping every index.
pub(super) fn blank_segments(
    command: &[char],
    segments: &[Segment],
    indices: impl IntoIterator<Item = usize>,
) -> Vec<char> {
    let mut chars = command.to_vec();
    for index in indices {
        let segment = segments[index];
        for slot in &mut chars[segment.start..segment.end] {
            *slot = ' ';
        }
    }
    chars
}
