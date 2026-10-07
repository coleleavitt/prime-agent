//! Length-preserving shell-text normalization ahead of the word scan: line
//! continuations folded, redirection words and here-document bodies masked,
//! unquoted backslash escapes stripped (with an index map back to the input),
//! and the quoting helpers the payload scans unwrap layers with.

use super::messages;
use super::patterns::{
    opens_comment_after, redirect_operator_at, static_target_end, RedirectOperator,
};
use super::pyos::is_space;

/// Collapse unquoted backslash-newline continuations between words into two
/// spaces (keeping indices aligned); an in-word pair is left for
/// [`strip_shell_escapes`] to drop, single-quoted pairs are data, and a
/// newline always ends a comment.
pub(super) fn normalize_line_continuations(command: &[char]) -> Vec<char> {
    let mut chars = command.to_vec();
    let n = chars.len();
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        if comment {
            if ch == '\n' {
                comment = false;
            }
        } else {
            match quote {
                None => {
                    if ch == '"' || ch == '\'' {
                        quote = Some(ch);
                    } else if ch == '#' && (i == 0 || opens_comment_after(chars[i - 1])) {
                        comment = true;
                    } else if ch == '\\' && i + 1 < n && chars[i + 1] == '\n' {
                        if i == 0
                            || opens_comment_after(chars[i - 1])
                            || matches!(chars[i - 1], '<' | '>')
                        {
                            chars[i] = ' ';
                            chars[i + 1] = ' ';
                        }
                        i += 1;
                    }
                }
                Some('\'') => {
                    if ch == '\'' {
                        quote = None;
                    }
                }
                Some(_) => {
                    if ch == '"' {
                        quote = None;
                    } else if ch == '\\' && i + 1 < n {
                        i += 1;
                    }
                }
            }
        }
        i += 1;
    }
    chars
}

/// Where a here-document after `operator` (`<<` or `<<-`) ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Heredoc {
    /// The delimiter text, `None` when it is missing or expandable.
    pub delimiter: Option<String>,
    /// The index just past the delimiter word (may pass the text end by one
    /// for an unterminated quoted delimiter).
    pub delimiter_end: usize,
    /// The end of the terminator line, `None` when no terminator matches
    /// (or the delimiter cannot be resolved statically).
    pub body_end: Option<usize>,
}

pub(super) fn locate_heredoc(command: &[char], operator: RedirectOperator) -> Heredoc {
    let n = command.len();
    let tabs = command.get(operator.end) == Some(&'-');
    let mut j = operator.end + usize::from(tabs);
    while j < n && is_space(command[j]) {
        j += 1;
    }
    let delim_start = j;
    let delimiter: String;
    if j < n && (command[j] == '\'' || command[j] == '"') {
        let quote_char = command[j];
        j += 1;
        while j < n && command[j] != quote_char {
            j += 1;
        }
        delimiter = command[delim_start + 1..j].iter().collect();
        j += 1;
    } else {
        let end = static_target_end(command, j);
        delimiter = command[j..end].iter().collect();
        j = end;
    }
    if delimiter.is_empty() || delimiter.contains(['$', '`', '\\']) {
        return Heredoc {
            delimiter: None,
            delimiter_end: j.min(n),
            body_end: None,
        };
    }
    let mut pos = j;
    while pos < n {
        let line_end = command[pos..]
            .iter()
            .position(|c| *c == '\n')
            .map(|offset| pos + offset);
        let mut line = &command[pos..line_end.unwrap_or(n)];
        if tabs {
            let skip = line.iter().take_while(|c| **c == '\t').count();
            line = &line[skip..];
        }
        if line.iter().copied().eq(delimiter.chars()) {
            return Heredoc {
                delimiter: Some(delimiter),
                delimiter_end: j,
                body_end: Some(line_end.unwrap_or(n)),
            };
        }
        match line_end {
            Some(end) => pos = end + 1,
            None => break,
        }
    }
    Heredoc {
        delimiter: Some(delimiter),
        delimiter_end: j,
        body_end: None,
    }
}

/// Blank out redirection words (operator plus a fully static target) and
/// here-document delimiters and bodies, keeping every index. Quoted data,
/// comments and substitutions stay live; redirections inside a
/// double-quoted backtick span are masked recursively. (A double-quoted
/// `$(...)` is not: the kernel's check for it compared a character list
/// with a string and never matched, so its interior stays as written, and
/// its redirections read like any other text.)
///
/// # Errors
///
/// The nesting refusal past [`messages::MAX_SUBSTITUTION_NESTING`] levels.
#[expect(
    clippy::too_many_lines,
    reason = "one left-to-right masking pass; its quote, comment, heredoc and target arms share the cursor"
)]
pub(super) fn mask_shell_redirections(command: &[char], depth: usize) -> Result<Vec<char>, String> {
    if depth > messages::MAX_SUBSTITUTION_NESTING {
        return Err(messages::nesting());
    }
    let mut chars = command.to_vec();
    let n = chars.len();
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        if comment {
            if ch == '\n' {
                comment = false;
            }
            i += 1;
            continue;
        }
        match quote {
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    i += 1;
                    continue;
                }
                if ch == '#' && (i == 0 || opens_comment_after(chars[i - 1])) {
                    comment = true;
                    i += 1;
                    continue;
                }
                if ch == '\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if let Some(operator) = redirect_operator_at(command, i) {
                    if operator.is_here_string(command) {
                        i = operator.end;
                        continue;
                    }
                    if operator.is_heredoc(command) {
                        let heredoc = locate_heredoc(command, operator);
                        for slot in &mut chars[operator.start..heredoc.delimiter_end.min(n)] {
                            *slot = ' ';
                        }
                        if let Some(body_end) = heredoc.body_end {
                            for slot in &mut chars[heredoc.delimiter_end..body_end] {
                                *slot = ' ';
                            }
                        }
                        i = heredoc.body_end.unwrap_or(heredoc.delimiter_end);
                        continue;
                    }
                    for slot in &mut chars[operator.start..operator.end] {
                        *slot = ' ';
                    }
                    i = operator.end;
                    let attached_end = static_target_end(command, i);
                    let (target_start, target_end) = if attached_end > i {
                        (i, attached_end)
                    } else if operator.duplicates {
                        (i, i)
                    } else {
                        let mut j = i;
                        while j < n && is_space(chars[j]) {
                            j += 1;
                        }
                        let detached_end = static_target_end(command, j);
                        if detached_end > j && j > i {
                            (j, detached_end)
                        } else {
                            (i, i)
                        }
                    };
                    for slot in &mut chars[target_start..target_end] {
                        *slot = ' ';
                    }
                    i = target_end;
                    continue;
                }
            }
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some(_) => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' && i + 1 < n {
                    i += 1;
                } else if ch == '`' {
                    let mut j = i + 1;
                    while j < n && chars[j] != '`' {
                        j += 1;
                    }
                    let interior = mask_shell_redirections(&command[i + 1..j], depth + 1)?;
                    chars[i + 1..j].copy_from_slice(&interior);
                    i = j;
                }
            }
        }
        i += 1;
    }
    Ok(chars)
}

/// Drop unquoted backslash escapes (and in-word line continuations, also
/// inside double quotes), returning the text and, per output character, the
/// input index it came from. Quoted and commented spans keep their
/// backslashes.
pub(super) fn strip_shell_escapes(command: &[char]) -> (Vec<char>, Vec<usize>) {
    let mut chars = Vec::with_capacity(command.len());
    let mut index_map = Vec::with_capacity(command.len());
    let n = command.len();
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut i = 0;
    while i < n {
        let ch = command[i];
        if comment {
            chars.push(ch);
            index_map.push(i);
            if ch == '\n' {
                comment = false;
            }
        } else if let Some(open) = quote {
            chars.push(ch);
            index_map.push(i);
            if open == '\'' {
                if ch == '\'' {
                    quote = None;
                }
            } else if ch == '"' {
                quote = None;
            } else if ch == '\\' && i + 1 < n && command[i + 1] == '\n' {
                chars.pop();
                index_map.pop();
                i += 1;
            } else if ch == '\\' && i + 1 < n {
                chars.push(command[i + 1]);
                index_map.push(i + 1);
                i += 1;
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
            chars.push(ch);
            index_map.push(i);
        } else if ch == '#' && (i == 0 || opens_comment_after(command[i - 1])) {
            comment = true;
            chars.push(ch);
            index_map.push(i);
        } else if ch == '\\' && i + 1 < n {
            if command[i + 1] != '\n' {
                chars.push(command[i + 1]);
                index_map.push(i + 1);
            }
            i += 1;
        } else {
            chars.push(ch);
            index_map.push(i);
        }
        i += 1;
    }
    (chars, index_map)
}

/// Remove the outermost quoting layer (quote characters become spaces, so
/// unquoting never joins words); inner quotes stay for the next layer.
pub(super) fn unquote_one_level(text: &[char]) -> Vec<char> {
    let mut chars = text.to_vec();
    let n = chars.len();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        match quote {
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    chars[i] = ' ';
                } else if ch == '\\' && i + 1 < n {
                    i += 1;
                }
            }
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                    chars[i] = ' ';
                }
            }
            Some(_) => {
                if ch == '"' {
                    quote = None;
                    chars[i] = ' ';
                } else if ch == '\\' && i + 1 < n {
                    i += 1;
                }
            }
        }
        i += 1;
    }
    chars
}

/// The character for a decoded code point; a surrogate (which a Rust string
/// cannot hold) folds to the replacement character, which matches no name.
fn code_point(value: u32) -> Option<char> {
    if value > 0x0010_FFFF {
        return None;
    }
    Some(char::from_u32(value).unwrap_or('\u{FFFD}'))
}

fn hex_run(body: &[char], from: usize, width: usize) -> String {
    body[from.min(body.len())..]
        .iter()
        .take(width)
        .take_while(|c| c.is_ascii_hexdigit())
        .collect()
}

/// Fold the escapes of a `$'...'` body the way bash does; an unknown escape
/// keeps its backslash.
pub(super) fn fold_ansi_c(body: &[char]) -> String {
    let mut out = String::new();
    let n = body.len();
    let mut i = 0;
    while i < n {
        let ch = body[i];
        if ch != '\\' || i + 1 >= n {
            out.push(ch);
            i += 1;
            continue;
        }
        let esc = body[i + 1];
        let simple = match esc {
            'a' => Some('\u{7}'),
            'b' => Some('\u{8}'),
            'e' | 'E' => Some('\u{1b}'),
            'f' => Some('\u{c}'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'v' => Some('\u{b}'),
            '\\' | '\'' | '"' | '?' | '`' => Some(esc),
            _ => None,
        };
        if let Some(simple) = simple {
            out.push(simple);
            i += 2;
            continue;
        }
        match esc {
            '0'..='7' => {
                let mut j = i + 2;
                let mut value = esc.to_digit(8).unwrap_or(0);
                while j < n && j < i + 4 && body[j].is_digit(8) {
                    value = value * 8 + body[j].to_digit(8).unwrap_or(0);
                    j += 1;
                }
                out.push(char::from_u32(value & 0xFF).unwrap_or('\u{FFFD}'));
                i = j;
            }
            'x' => {
                let digits = hex_run(body, i + 2, 2);
                if digits.is_empty() {
                    out.push_str("\\x");
                    i += 2;
                } else {
                    let value = u32::from_str_radix(&digits, 16).unwrap_or(0);
                    out.push(code_point(value).unwrap_or('\u{FFFD}'));
                    i += 2 + digits.len();
                }
            }
            'c' => {
                if let Some(next) = body.get(i + 2) {
                    out.push(if *next == '?' {
                        '\u{7f}'
                    } else {
                        char::from_u32(u32::from(*next) & 0x1F).unwrap_or('\u{FFFD}')
                    });
                    i += 3;
                } else {
                    out.push_str("\\c");
                    i += 2;
                }
            }
            'u' | 'U' => {
                let width = if esc == 'u' { 4 } else { 8 };
                let digits = hex_run(body, i + 2, width);
                if digits.is_empty() {
                    out.push('\\');
                    out.push(esc);
                    i += 2;
                } else {
                    if let Some(decoded) =
                        u32::from_str_radix(&digits, 16).ok().and_then(code_point)
                    {
                        out.push(decoded);
                    } else {
                        out.push('\\');
                        out.push(esc);
                        out.push_str(&digits);
                    }
                    i += 2 + digits.len();
                }
            }
            _ => {
                out.push('\\');
                out.push(esc);
                i += 2;
            }
        }
    }
    out
}

/// Fold the `$'...'` starting at `i` (the `$`), bounded by `end`: the folded
/// text and the index just past the closing quote (past `end` when
/// unterminated).
pub(super) fn fold_ansi_c_span(text: &[char], i: usize, end: usize) -> (String, usize) {
    let mut j = i + 2;
    while j < end && text[j] != '\'' {
        j += if text[j] == '\\' { 2 } else { 1 };
    }
    let body_end = j.min(end).max(i + 2).min(text.len());
    (fold_ansi_c(&text[(i + 2).min(body_end)..body_end]), j + 1)
}

/// Fold every `$'...'` span to its content and every `$"` to `"`, so a
/// payload scans like the string bash hands the wrapper.
pub(super) fn expand_ansi_c_payloads(text: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    let n = text.len();
    let mut i = 0;
    while i < n {
        let ch = text[i];
        if ch == '$' && text.get(i + 1) == Some(&'\'') {
            let (folded, next) = fold_ansi_c_span(text, i, n);
            out.extend(folded.chars());
            i = next;
            continue;
        }
        if ch == '$' && text.get(i + 1) == Some(&'"') {
            out.push('"');
            i += 2;
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}
