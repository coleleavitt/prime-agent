//! Length-preserving normalization and masking of the command text.
//!
//! The discard patterns run on text where line continuations are joined,
//! redirections are blanked, unquoted escapes are folded, and quoted data and
//! comments are masked, with every pass keeping (or mapping) character
//! positions so a match points back into the command as written. Positions
//! are `char` indices, the Python guard's string indices.

use std::sync::LazyLock;

use super::pattern::{is_space, Pattern};

/// True when the `#` at `index` opens a comment: the shell starts a comment
/// only at the beginning of a word, so `foo#bar` is literal text.
pub(super) fn starts_comment(text: &[char], index: usize) -> bool {
    text[index] == '#'
        && (index == 0 || is_space(text[index - 1]) || ";&|(){}".contains(text[index - 1]))
}

/// Collapse unquoted backslash-newline continuations to two spaces, so
/// `git reset \` + newline + `--hard` scans as one command. Single-quoted
/// pairs are data and a newline always ends a comment.
pub(super) fn normalize_line_continuations(command: &[char]) -> Vec<char> {
    let mut chars = command.to_vec();
    let mut quote: Option<char> = None;
    let mut comment = false;
    let n = chars.len();
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
                    } else if starts_comment(&chars, i) {
                        comment = true;
                    } else if ch == '\\' && i + 1 < n && chars[i + 1] == '\n' {
                        chars[i] = ' ';
                        chars[i + 1] = ' ';
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

/// Index one past the backtick closing the one at `start` (a backslash
/// escapes the next character), or `limit` when it never closes.
pub(super) fn backtick_end(text: &[char], start: usize, limit: usize) -> usize {
    let mut i = start + 1;
    while i < limit {
        if text[i] == '\\' && i + 1 < limit {
            i += 2;
            continue;
        }
        if text[i] == '`' {
            return i + 1;
        }
        i += 1;
    }
    limit
}

/// Index of the `)` closing the `$(` whose `(` is at `start`: only an
/// unquoted `)` closes it, quoting inside starts fresh, and parentheses nest.
/// `limit` when it never closes.
pub(super) fn substitution_end(text: &[char], start: usize, limit: usize) -> usize {
    let mut depth = 0i64;
    let mut quote: Option<char> = None;
    let mut i = start;
    while i < limit {
        let ch = text[i];
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                }
            }
            Some(_) => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' && i + 1 < limit {
                    i += 1;
                } else if ch == '$' && text.get(i + 1) == Some(&'(') {
                    i = substitution_end(text, i + 1, limit) - 1;
                } else if ch == '`' {
                    i = backtick_end(text, i, limit) - 1;
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                } else if ch == '\\' && i + 1 < limit {
                    i += 1;
                } else if ch == '`' {
                    i = backtick_end(text, i, limit) - 1;
                } else if ch == '(' {
                    depth += 1;
                } else if ch == ')' {
                    depth -= 1;
                    if depth == 0 {
                        return i;
                    }
                }
            }
        }
        i += 1;
    }
    limit
}

/// A heredoc's delimiter word, read after the `<<` operator ending at `start`.
struct HeredocDelimiter {
    word_start: usize,
    word_end: usize,
    delimiter: Vec<char>,
    /// An unquoted delimiter: the body still expands substitutions.
    expands: bool,
}

/// The delimiter word after `<<`, or `None` when the shell would build it
/// from a variable or a substitution (its body then stays live).
fn heredoc_delimiter(command: &[char], start: usize) -> Option<HeredocDelimiter> {
    let n = command.len();
    let mut i = start;
    while i < n && is_space(command[i]) {
        i += 1;
    }
    let word_start = i;
    while i < n && !is_space(command[i]) && !";&|<>()".contains(command[i]) {
        i += 1;
    }
    let word = &command[word_start..i];
    if word.is_empty() || word.contains(&'$') || word.contains(&'`') {
        return None;
    }
    let (delimiter, expands) =
        if word.len() > 2 && (word[0] == '\'' || word[0] == '"') && word[word.len() - 1] == word[0]
        {
            (word[1..word.len() - 1].to_vec(), false)
        } else if word[0] == '\\' {
            (word[1..].to_vec(), false)
        } else {
            (word.to_vec(), true)
        };
    Some(HeredocDelimiter {
        word_start,
        word_end: i,
        delimiter,
        expands,
    })
}

fn find_from(text: &[char], needle: char, from: usize) -> Option<usize> {
    text.get(from..)?
        .iter()
        .position(|ch| *ch == needle)
        .map(|offset| from + offset)
}

/// Just past the line ending a heredoc body whose operator line ends at the
/// newline `line_end`, or `None` when no line is exactly the delimiter (tab
/// stripped for `<<-`).
fn heredoc_body_end(
    command: &[char],
    line_end: usize,
    delimiter: &[char],
    strip_tabs: bool,
) -> Option<usize> {
    let mut pos = find_from(command, '\n', line_end);
    while let Some(at) = pos {
        let line_stop = find_from(command, '\n', at + 1);
        let line = &command[at + 1..line_stop.unwrap_or(command.len())];
        let line = if strip_tabs {
            let skip = line.iter().take_while(|ch| **ch == '\t').count();
            &line[skip..]
        } else {
            line
        };
        if line == delimiter {
            return Some(line_stop.unwrap_or(command.len()));
        }
        pos = line_stop;
    }
    None
}

/// Blank heredoc data: an unquoted delimiter keeps substitutions live (they
/// execute); a quoted one makes the whole body inert.
fn mask_heredoc_body(
    chars: &mut [char],
    command: &[char],
    start: usize,
    end: usize,
    expands: bool,
) {
    if !expands {
        chars[start..end].fill(' ');
        return;
    }
    let mut i = start;
    while i < end {
        let ch = command[i];
        if ch == '$' && command.get(i + 1) == Some(&'(') {
            i = substitution_end(command, i + 1, end) + 1;
        } else if ch == '`' {
            i = backtick_end(command, i, end);
        } else {
            chars[i] = ' ';
            i += 1;
        }
    }
}

static REDIRECT_OPERATOR: LazyLock<Pattern> =
    LazyLock::new(|| Pattern::new(r"(?:&>{1,2}|>&|[0-9]*[<>]{1,3}(&[0-9]+)?)"));
static STATIC_REDIRECT_TARGET: LazyLock<Pattern> =
    LazyLock::new(|| Pattern::new(r#"[^\s;&|<>()$`"']*"#));

/// Blank shell redirection words, keeping positions: `git reset 2>/dev/null
/// --hard` scans as `git reset --hard`. Only the operator and a fully static
/// target are masked; quoted data, comments and substitutions stay live, and
/// heredoc bodies are blanked as data.
#[expect(
    clippy::too_many_lines,
    reason = "one quote-aware pass; the redirect and heredoc arms share its state"
)]
pub(super) fn mask_shell_redirections(command: &[char]) -> Vec<char> {
    let mut chars = command.to_vec();
    let mut quote: Option<char> = None;
    let mut comment = false;
    let n = chars.len();
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
                if starts_comment(&chars, i) {
                    comment = true;
                    i += 1;
                    continue;
                }
                if ch == '\\' && i + 1 < n {
                    i += 2;
                    continue;
                }
                if let Some(operator) = REDIRECT_OPERATOR.match_at(command, i) {
                    chars[operator.start()..operator.end()].fill(' ');
                    i = operator.end();
                    if command[operator.start()..operator.end()] == ['<', '<'] {
                        let tabbed = command.get(i) == Some(&'-');
                        if tabbed {
                            chars[i] = ' ';
                        }
                        if let Some(heredoc) =
                            heredoc_delimiter(command, if tabbed { i + 1 } else { i })
                        {
                            chars[heredoc.word_start..heredoc.word_end].fill(' ');
                            if let Some(line_end) = find_from(command, '\n', heredoc.word_end) {
                                if let Some(body_end) =
                                    heredoc_body_end(command, line_end, &heredoc.delimiter, tabbed)
                                {
                                    mask_heredoc_body(
                                        &mut chars,
                                        command,
                                        line_end + 1,
                                        body_end,
                                        heredoc.expands,
                                    );
                                }
                            }
                            i = heredoc.word_end;
                            continue;
                        }
                    }
                    let attached_end = STATIC_REDIRECT_TARGET
                        .match_at(command, i)
                        .map_or(i, |found| found.end());
                    let (target_start, target_end) = if attached_end > i {
                        (i, attached_end)
                    } else if operator.group(1).is_some() {
                        (i, i)
                    } else {
                        let mut j = i;
                        while j < n && is_space(chars[j]) {
                            j += 1;
                        }
                        let detached_end = STATIC_REDIRECT_TARGET
                            .match_at(command, j)
                            .map_or(j, |found| found.end());
                        if detached_end > j && j > i {
                            (j, detached_end)
                        } else {
                            (i, i)
                        }
                    };
                    chars[target_start..target_end].fill(' ');
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
                } else if ch == '$' && chars.get(i + 1) == Some(&'(') {
                    let close = substitution_end(command, i + 1, n);
                    if close < n {
                        let interior = mask_shell_redirections(&command[i + 2..close]);
                        chars[i + 2..close].copy_from_slice(&interior);
                        i = close;
                    }
                } else if ch == '`' {
                    let close = backtick_end(command, i, n);
                    if close < n {
                        let interior = mask_shell_redirections(&command[i + 1..close - 1]);
                        chars[i + 1..close - 1].copy_from_slice(&interior);
                        i = close - 1;
                    }
                }
            }
        }
        i += 1;
    }
    chars
}

/// Drop unquoted backslash escapes (`g\it` scans as `git`), mapping each
/// output character back to its input index. Quoted and commented spans keep
/// their backslashes.
pub(super) fn strip_shell_escapes(command: &[char]) -> (Vec<char>, Vec<usize>) {
    let mut chars = Vec::with_capacity(command.len());
    let mut index_map = Vec::with_capacity(command.len());
    let mut quote: Option<char> = None;
    let mut comment = false;
    let n = command.len();
    let mut i = 0;
    while i < n {
        let ch = command[i];
        if comment {
            chars.push(ch);
            index_map.push(i);
            if ch == '\n' {
                comment = false;
            }
            i += 1;
        } else if quote.is_none() {
            if ch == '"' || ch == '\'' {
                quote = Some(ch);
                chars.push(ch);
                index_map.push(i);
            } else if ch == '#'
                && (i == 0 || is_space(command[i - 1]) || ";&|(){}".contains(command[i - 1]))
            {
                comment = true;
                chars.push(ch);
                index_map.push(i);
            } else if ch == '\\' && i + 1 < n && command[i + 1] != '\n' {
                chars.push(command[i + 1]);
                index_map.push(i + 1);
                i += 1;
            } else {
                chars.push(ch);
                index_map.push(i);
            }
            i += 1;
        } else {
            chars.push(ch);
            index_map.push(i);
            if quote == Some('\'') {
                if ch == '\'' {
                    quote = None;
                }
            } else if ch == '"' {
                quote = None;
            } else if ch == '\\' && i + 1 < n {
                chars.push(command[i + 1]);
                index_map.push(i + 1);
                i += 1;
            }
            i += 1;
        }
    }
    (chars, index_map)
}

/// [`strip_shell_escapes`] without the index map.
pub(super) fn stripped(command: &[char]) -> Vec<char> {
    strip_shell_escapes(command).0
}

/// The full normalization the scans read: continuations joined, redirections
/// masked, escapes stripped.
pub(super) fn normalized(command: &[char]) -> (Vec<char>, Vec<usize>) {
    strip_shell_escapes(&mask_shell_redirections(&normalize_line_continuations(
        command,
    )))
}

/// Blank quoted data and comments, keeping positions; substitutions stay
/// live (they execute), with quoted data inside them masked in turn.
pub(super) fn mask_quoted_spans(command: &[char]) -> Vec<char> {
    let mut chars = command.to_vec();
    let mut quote: Option<char> = None;
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        match quote {
            None => {
                if starts_comment(&chars, i) {
                    let mut j = i;
                    while j < n && chars[j] != '\n' {
                        chars[j] = ' ';
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                }
            }
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    chars[i] = ' ';
                }
            }
            Some(_) => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' && i + 1 < n {
                    chars[i] = ' ';
                    chars[i + 1] = ' ';
                    i += 1;
                } else if ch == '$' && i + 1 < n && chars[i + 1] == '(' {
                    let close = substitution_end(command, i + 1, n);
                    if close < n {
                        let interior = mask_quoted_spans(&command[i + 2..close]);
                        chars[i + 2..close].copy_from_slice(&interior);
                        i = close - 1;
                    }
                } else if ch == '`' {
                    let close = backtick_end(command, i, n);
                    if close < n {
                        let interior = mask_quoted_spans(&command[i + 1..close - 1]);
                        chars[i + 1..close - 1].copy_from_slice(&interior);
                        i = close - 1;
                    }
                } else {
                    chars[i] = ' ';
                }
            }
        }
        i += 1;
    }
    chars
}

/// Remove the outermost quoting layer, turning the quote characters into
/// spaces (so unquoting never joins words); inner quotes stay.
pub(super) fn unquote_one_level(text: &[char]) -> Vec<char> {
    let mut chars = text.to_vec();
    let mut quote: Option<char> = None;
    let n = chars.len();
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

/// Spans of the text inside each `$(...)` and backtick substitution.
pub(super) fn substitution_interiors(text: &[char]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let n = text.len();
    let mut i = 0;
    while i < n {
        if text[i] == '$' && text.get(i + 1) == Some(&'(') {
            let close = substitution_end(text, i + 1, n);
            spans.push((i + 2, close));
            i = close;
        } else if text[i] == '`' {
            let close = backtick_end(text, i, n);
            spans.push((i + 1, close - 1));
            i = close;
        } else {
            i += 1;
        }
    }
    spans
}

/// Python `str.strip()`.
pub(super) fn trim(text: &[char]) -> &[char] {
    let start = text
        .iter()
        .position(|ch| !is_space(*ch))
        .unwrap_or(text.len());
    let end = text
        .iter()
        .rposition(|ch| !is_space(*ch))
        .map_or(start, |end| end + 1);
    &text[start..end.max(start)]
}

/// Python `re.split(r"\s+", text)`: empty pieces at the ends are kept.
pub(super) fn split_whitespace_runs(text: &[char]) -> Vec<&[char]> {
    let mut pieces = Vec::new();
    let mut cursor = 0;
    let mut i = 0;
    while i < text.len() {
        if is_space(text[i]) {
            pieces.push(&text[cursor..i]);
            while i < text.len() && is_space(text[i]) {
                i += 1;
            }
            cursor = i;
        } else {
            i += 1;
        }
    }
    pieces.push(&text[cursor..]);
    pieces
}

/// The non-empty whitespace-separated tokens.
pub(super) fn tokens(text: &[char]) -> Vec<&[char]> {
    split_whitespace_runs(text)
        .into_iter()
        .filter(|token| !token.is_empty())
        .collect()
}

pub(super) fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

pub(super) fn string(text: &[char]) -> String {
    text.iter().collect()
}

pub(super) fn starts_with(text: &[char], prefix: &str) -> bool {
    prefix
        .chars()
        .enumerate()
        .all(|(at, ch)| text.get(at) == Some(&ch))
}

pub(super) fn equals(text: &[char], other: &str) -> bool {
    text.iter().copied().eq(other.chars())
}

/// Python's `substring in text`.
pub(super) fn contains(text: &[char], needle: &str) -> bool {
    let needle = chars(needle);
    needle.is_empty()
        || text
            .windows(needle.len())
            .any(|window| window == needle.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(f: fn(&[char]) -> Vec<char>, text: &str) -> String {
        string(&f(&chars(text)))
    }

    #[test]
    fn redirections_and_heredoc_bodies_are_blanked_in_place() {
        assert_eq!(
            run(mask_shell_redirections, "git reset 2>/dev/null --hard"),
            "git reset             --hard"
        );
        assert_eq!(
            run(mask_shell_redirections, "git reset 2>&1 --hard"),
            "git reset      --hard"
        );
        assert_eq!(
            run(
                mask_shell_redirections,
                "cat <<EOF\ngit reset --hard\nEOF\nx"
            ),
            "cat      \n                    \nx"
        );
        assert_eq!(
            run(mask_shell_redirections, "cat <<EOF\n$(git x)\nEOF"),
            "cat      \n$(git x)    "
        );
    }

    #[test]
    fn quoted_data_and_comments_are_masked_but_substitutions_stay_live() {
        assert_eq!(run(mask_quoted_spans, "echo 'git' # c"), "echo '   '    ");
        assert_eq!(
            run(mask_quoted_spans, "echo \"$(git 'x')\""),
            "echo \"$(git ' ' \""
        );
        assert_eq!(
            strip_shell_escapes(&chars("g\\it 'a\\b'")),
            (chars("git 'a\\b'"), vec![0, 2, 3, 4, 5, 6, 7, 8, 9])
        );
    }
}
