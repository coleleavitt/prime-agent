//! The text passes that turn a script into what the shell builds argv from:
//! line continuations joined, redirections blanked, unquoted escapes folded,
//! one quoting layer peeled, ANSI-C bodies decoded, and the matching close of
//! a substitution found.
//!
//! Every pass but continuation joining and escape folding keeps character
//! positions, so word spans stay comparable across them.

use super::budget::{Budget, Scan};
use super::text::{chars, is_space};

/// Whether a `#` at `index` starts a comment: at the start of the text or
/// after whitespace or a control character.
fn opens_comment(text: &[char], index: usize) -> bool {
    index == 0 || {
        let previous = text[index - 1];
        is_space(previous) || ";&|(){}".contains(previous)
    }
}

/// Remove backslash-newline line continuations the way the shell does.
///
/// The shell deletes the pair and joins what surrounds it (`ma\<newline>in`
/// is the single word `main`). Single-quoted backslash-newlines are literal
/// data and a newline always ends a comment, so those stay; inside double
/// quotes the pair is dropped too and any other escape is kept whole (so a
/// `\"` does not end the string).
pub(super) fn normalize_continuations(command: &str) -> String {
    let text = chars(command);
    let n = text.len();
    let mut out = String::with_capacity(command.len());
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut i = 0;
    while i < n {
        let ch = text[i];
        if comment {
            out.push(ch);
            if ch == '\n' {
                comment = false;
            }
        } else if let Some(open) = quote {
            if open == '\'' {
                out.push(ch);
                if ch == '\'' {
                    quote = None;
                }
            } else if ch == '\\' && i + 1 < n {
                if text[i + 1] != '\n' {
                    out.push(ch);
                    out.push(text[i + 1]);
                }
                i += 2;
                continue;
            } else {
                out.push(ch);
                if ch == '"' {
                    quote = None;
                }
            }
        } else {
            if ch == '"' || ch == '\'' {
                quote = Some(ch);
            } else if ch == '#' && opens_comment(&text, i) {
                comment = true;
            }
            if ch == '\\' && text.get(i + 1) == Some(&'\n') {
                i += 2;
                continue;
            }
            out.push(ch);
        }
        i += 1;
    }
    out
}

/// A redirection operator at `index` (`_FP_REDIRECT_OPERATOR`): `&>`/`&>>`,
/// `>&`, or optional fd digits, one to three `<`/`>`, and an optional `&fd`
/// duplication. Returns its end and whether it carries the duplication (which
/// is its own target).
fn redirect_operator(text: &[char], index: usize) -> Option<(usize, bool)> {
    let at = |offset: usize| text.get(index + offset).copied();
    if at(0) == Some('&') && at(1) == Some('>') {
        return Some((index + if at(2) == Some('>') { 3 } else { 2 }, false));
    }
    if at(0) == Some('>') && at(1) == Some('&') {
        return Some((index + 2, false));
    }
    let digits = text[index..]
        .iter()
        .take_while(|ch| ch.is_ascii_digit())
        .count();
    let arrows = text[index + digits..]
        .iter()
        .take(3)
        .take_while(|ch| **ch == '<' || **ch == '>')
        .count();
    if arrows == 0 {
        return None;
    }
    let end = index + digits + arrows;
    if text.get(end) == Some(&'&') {
        let fd = text[end + 1..]
            .iter()
            .take_while(|ch| ch.is_ascii_digit())
            .count();
        if fd > 0 {
            return Some((end + 1 + fd, true));
        }
    }
    Some((end, false))
}

/// The end of a static redirection target starting at `index`
/// (`_FP_STATIC_REDIRECT_TARGET`): no whitespace, control, quote or
/// substitution character.
fn static_target_end(text: &[char], index: usize) -> usize {
    index
        + text[index.min(text.len())..]
            .iter()
            .take_while(|ch| !is_space(**ch) && !";&|<>()$`\"'".contains(**ch))
            .count()
}

/// Blank out shell redirection words, keeping character positions.
///
/// The shell consumes redirections before git sees its argv, so `git push
/// 2>/dev/null -f origin main` must scan as `git push -f origin main`. Only
/// the operator and a fully static attached or next-word target are masked;
/// quoted data, comments, and substitutions stay live, and a substitution
/// inside double quotes is masked on its own (one budget unit per interior).
pub(super) fn mask_redirections(command: &str, budget: &Budget) -> Scan<String> {
    let text = chars(command);
    Ok(mask_chars(&text, budget)?.into_iter().collect())
}

fn mask_chars(text: &[char], budget: &Budget) -> Scan<Vec<char>> {
    let mut out = text.to_vec();
    let n = text.len();
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut i = 0;
    while i < n {
        let ch = text[i];
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
                if ch == '#' && opens_comment(text, i) {
                    comment = true;
                    i += 1;
                    continue;
                }
                if ch == '\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if let Some((operator_end, duplicates)) = redirect_operator(text, i) {
                    out[i..operator_end].fill(' ');
                    let attached_end = static_target_end(text, operator_end);
                    let (target_start, target_end) = if attached_end > operator_end {
                        (operator_end, attached_end)
                    } else if duplicates {
                        (operator_end, operator_end)
                    } else {
                        let next = operator_end
                            + text[operator_end..]
                                .iter()
                                .take_while(|ch| is_space(**ch))
                                .count();
                        let detached_end = static_target_end(text, next);
                        if detached_end > next && next > operator_end {
                            (next, detached_end)
                        } else {
                            (operator_end, operator_end)
                        }
                    };
                    out[target_start..target_end].fill(' ');
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
                } else if ch == '$' && text.get(i + 1) == Some(&'(') {
                    let close = matching_paren(text, i + 1, n);
                    budget.enter()?;
                    if close > i + 2 {
                        let interior = mask_chars(&text[i + 2..close], budget)?;
                        out[i + 2..close].copy_from_slice(&interior);
                    }
                    budget.leave();
                    i = close;
                } else if ch == '`' {
                    let close = matching_backtick(text, i, n);
                    budget.enter()?;
                    if close > i + 1 {
                        let interior = mask_chars(&text[i + 1..close], budget)?;
                        out[i + 1..close].copy_from_slice(&interior);
                    }
                    budget.leave();
                    i = close;
                }
            }
        }
        i += 1;
    }
    Ok(out)
}

/// Remove unquoted backslash escapes, mapping each output character back to
/// its input index. An unquoted `\X` is the literal X (`g\it` is `git`);
/// quoted and commented spans keep their backslashes.
pub(super) fn strip_escapes(command: &str) -> (Vec<char>, Vec<usize>) {
    let text = chars(command);
    let n = text.len();
    let mut out = Vec::with_capacity(n);
    let mut index_map = Vec::with_capacity(n);
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut i = 0;
    while i < n {
        let ch = text[i];
        if comment {
            out.push(ch);
            index_map.push(i);
            if ch == '\n' {
                comment = false;
            }
        } else if let Some(open) = quote {
            out.push(ch);
            index_map.push(i);
            if open == '\'' {
                if ch == '\'' {
                    quote = None;
                }
            } else if ch == '"' {
                quote = None;
            } else if ch == '\\' && i + 1 < n {
                out.push(text[i + 1]);
                index_map.push(i + 1);
                i += 1;
            }
        } else if ch == '"' || ch == '\'' {
            quote = Some(ch);
            out.push(ch);
            index_map.push(i);
        } else if ch == '#' && opens_comment(&text, i) {
            comment = true;
            out.push(ch);
            index_map.push(i);
        } else if ch == '\\' && i + 1 < n && text[i + 1] != '\n' {
            out.push(text[i + 1]);
            index_map.push(i + 1);
            i += 1;
        } else {
            out.push(ch);
            index_map.push(i);
        }
        i += 1;
    }
    (out, index_map)
}

/// The script normalized the way every scan reads it: continuations joined,
/// redirections masked, unquoted escapes folded.
pub(super) fn prepare(command: &str, budget: &Budget) -> Scan<(String, Vec<char>, Vec<usize>)> {
    let resolved = mask_redirections(&normalize_continuations(command), budget)?;
    let (normalized, index_map) = strip_escapes(&resolved);
    Ok((resolved, normalized, index_map))
}

/// Remove the outermost quoting layer. Inner quotes stay quoted so the next
/// scan layer still treats them as data; the removed quote characters become
/// spaces so unquoting never joins separate words.
pub(super) fn unquote_one_level(text: &str) -> String {
    let mut out = chars(text);
    let n = out.len();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < n {
        let ch = out[i];
        match quote {
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    out[i] = ' ';
                } else if ch == '\\' && i + 1 < n {
                    i += 1;
                }
            }
            // Inside quotes a backslash escapes nothing at this layer: the
            // next matching quote closes the span.
            Some(open) if ch == open => {
                quote = None;
                out[i] = ' ';
            }
            Some(_) => {}
        }
        i += 1;
    }
    out.into_iter().collect()
}

/// `text` with every quoted or backslash-escaped span blanked out. With
/// `keep_double_quoted`, the content of a double-quoted span stays (the shell
/// still expands `$` and backticks there); escapes are blanked either way and
/// single-quoted spans stay data.
pub(super) fn unquoted_text(text: &str, keep_double_quoted: bool) -> String {
    let source = chars(text);
    let mut out = source.clone();
    let n = source.len();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < n {
        let ch = source[i];
        match quote {
            None => {
                if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                    out[i] = ' ';
                } else if ch == '\\' && i + 1 < n {
                    out[i] = ' ';
                    out[i + 1] = ' ';
                    i += 1;
                }
            }
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
                out[i] = ' ';
            }
            Some(_) => {
                if ch == '\\' && i + 1 < n {
                    out[i] = ' ';
                    out[i + 1] = ' ';
                    i += 1;
                } else if keep_double_quoted {
                    if ch == '"' {
                        quote = None;
                        out[i] = ' ';
                    }
                } else {
                    if ch == '"' {
                        quote = None;
                    }
                    out[i] = ' ';
                }
            }
        }
        i += 1;
    }
    out.into_iter().collect()
}

/// A decoded code point, bounded the way the guard reads one: past U+10FFFF
/// it clamps, and a surrogate (which no Rust `char` holds) reads as U+FFFD.
fn code_point(value: u32) -> char {
    char::from_u32(value.min(0x10_FFFF)).unwrap_or('\u{FFFD}')
}

/// Decode the body of a `$'...'` word. Unknown escapes resolve to the
/// escaped character itself (`$'ma\in'` reads `main`), so a protected name
/// cannot hide behind one; `\x`/`\u`/`\U` without digits read as the letter.
pub(super) fn ansi_c_decoded(body: &str) -> String {
    let body = chars(body);
    let n = body.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        let ch = body[i];
        if ch != '\\' || i + 1 >= n {
            out.push(ch);
            i += 1;
            continue;
        }
        let escape = body[i + 1];
        i += 2;
        let simple = match escape {
            'a' => Some('\u{7}'),
            'b' => Some('\u{8}'),
            'e' | 'E' => Some('\u{1b}'),
            'f' => Some('\u{c}'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'v' => Some('\u{b}'),
            '\\' | '\'' | '"' | '?' => Some(escape),
            _ => None,
        };
        if let Some(simple) = simple {
            out.push(simple);
            continue;
        }
        if escape.is_digit(8) {
            let mut value = escape.to_digit(8).unwrap_or(0);
            let mut digits = 1;
            while digits < 3 && i < n && body[i].is_digit(8) {
                value = value * 8 + body[i].to_digit(8).unwrap_or(0);
                digits += 1;
                i += 1;
            }
            out.push(code_point(value & 0xFF));
            continue;
        }
        if let Some(width) = match escape {
            'x' => Some(2),
            'u' => Some(4),
            'U' => Some(8),
            _ => None,
        } {
            let mut value: u64 = 0;
            let mut digits = 0;
            while digits < width && i < n && body[i].is_ascii_hexdigit() {
                value = value * 16 + u64::from(body[i].to_digit(16).unwrap_or(0));
                digits += 1;
                i += 1;
            }
            if digits == 0 {
                out.push(escape);
            } else {
                out.push(code_point(u32::try_from(value).unwrap_or(u32::MAX)));
            }
            continue;
        }
        if escape == 'c' && i < n {
            let upper = body[i].to_uppercase().next().unwrap_or(body[i]);
            out.push(code_point(u32::from(upper) & 0x1F));
            i += 1;
            continue;
        }
        out.push(escape);
    }
    out
}

/// Index of the `)` matching the `(` at `open`, or `end - 1`. A `)` inside
/// quotes (single, double, or ANSI-C `$'...'`, where `\'` is escaped) or
/// behind a backslash is data, so `"$(printf ')'; git push -f origin main)"`
/// closes at the last `)`. An unterminated substitution reports the last
/// character, so its whole tail is scanned.
pub(super) fn matching_paren(text: &[char], open: usize, end: usize) -> usize {
    #[derive(Clone, Copy, PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
        AnsiC,
    }
    let mut depth = 0isize;
    let mut quote = Quote::None;
    let mut i = open;
    while i < end {
        let ch = text[i];
        match quote {
            Quote::None => {
                if ch == '\\' && i + 1 < end {
                    i += 2;
                    continue;
                }
                if ch == '$' && i + 1 < end && text[i + 1] == '\'' {
                    quote = Quote::AnsiC;
                    i += 2;
                    continue;
                }
                match ch {
                    '\'' => quote = Quote::Single,
                    '"' => quote = Quote::Double,
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            return i;
                        }
                    }
                    _ => {}
                }
            }
            Quote::AnsiC | Quote::Double if ch == '\\' && i + 1 < end => {
                i += 2;
                continue;
            }
            Quote::AnsiC | Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                }
            }
            Quote::Double => {
                if ch == '"' {
                    quote = Quote::None;
                }
            }
        }
        i += 1;
    }
    end.saturating_sub(1)
}

/// Index of the backtick closing the one at `open`, or `end - 1`. Bash ends
/// an old-style substitution at the first backtick a backslash does not
/// escape: quotes inside the backquotes do not protect one.
pub(super) fn matching_backtick(text: &[char], open: usize, end: usize) -> usize {
    let mut i = open + 1;
    while i < end {
        if text[i] == '\\' && i + 1 < end {
            i += 2;
            continue;
        }
        if text[i] == '`' {
            return i;
        }
        i += 1;
    }
    end.saturating_sub(1)
}

/// Open and close parens outside quotes and escapes (the same quoting
/// [`matching_paren`] follows): `echo "("` opens no group.
pub(super) fn unquoted_paren_counts(text: &str) -> (usize, usize) {
    let text = chars(text);
    let end = text.len();
    let (mut opens, mut closes) = (0, 0);
    let mut quote: Option<char> = None;
    let mut ansi = false;
    let mut i = 0;
    while i < end {
        let ch = text[i];
        if ansi {
            if ch == '\\' && i + 1 < end {
                i += 2;
                continue;
            }
            if ch == '\'' {
                ansi = false;
            }
        } else if let Some(open) = quote {
            if open == '\'' {
                if ch == '\'' {
                    quote = None;
                }
            } else if ch == '\\' && i + 1 < end {
                i += 2;
                continue;
            } else if ch == '"' {
                quote = None;
            }
        } else {
            if ch == '\\' && i + 1 < end {
                i += 2;
                continue;
            }
            if ch == '$' && i + 1 < end && text[i + 1] == '\'' {
                ansi = true;
                i += 2;
                continue;
            }
            match ch {
                '\'' | '"' => quote = Some(ch),
                '(' => opens += 1,
                ')' => closes += 1,
                _ => {}
            }
        }
        i += 1;
    }
    (opens, closes)
}
