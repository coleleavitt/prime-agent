//! The shell's reading of one segment's words: quote removal, escapes,
//! ANSI-C (`$'...'`) and locale (`$"..."`) quoting, kept backtick spans, and
//! the redirection and assignment words the dump checks drop.
//!
//! Text is handled as `char` slices so every index is a character position
//! of the command, exactly as the masks and segments count them.

/// Python's `str.isdigit` (and the regex `\d`) for the characters a command
/// carries: ASCII digits, plus the non-ASCII numeric characters.
pub(super) fn is_digit(ch: char) -> bool {
    ch.is_ascii_digit() || (!ch.is_ascii() && ch.is_numeric())
}

/// `str.isdigit()` on a whole string: non-empty and every character a digit.
pub(super) fn all_digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(is_digit)
}

/// Python's `str.isspace`: Unicode white space plus the four information
/// separators (`\x1c`-`\x1f`) Python also splits on.
fn is_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

/// Python's `str.split()` with no separator: runs of white space split, and
/// leading or trailing white space yields no empty word.
pub(super) fn split_blanks(text: &str) -> Vec<String> {
    text.split(is_space)
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// One word the shell builds, with the source span it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellWord {
    pub text: String,
    pub start: usize,
    /// One past the last character read; can pass the scanned end when a
    /// span ran over it (the readers clamp it).
    pub end: usize,
}

/// How much of a slice [`shell_words`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reach {
    /// Every word.
    All,
    /// Only the first word: reading one word costs that word's length, so a
    /// line of thousands of here-document openers is not lexed once per
    /// opener.
    FirstWord,
}

/// Split `command[start..end]` into words the way the shell builds them.
///
/// Whitespace separates words only outside quotes, quotes are removed from
/// the words they build (`"env"` runs env, `ca"t"` runs cat), and a backslash
/// makes the next character a literal part of the word. `$'...'` and `$"..."`
/// drop their `$`. A `#` at a word boundary starts a comment that ends the
/// slice. An unbalanced quote ends at the slice end, and a quoted span still
/// emits a word even when it is empty (`grep ''`). What a backtick span holds
/// is one lexer token, kept as written: its blanks do not separate words.
pub(super) fn shell_words(
    command: &[char],
    start: usize,
    end: usize,
    reach: Reach,
) -> Vec<ShellWord> {
    let mut words = Vec::new();
    let mut chars = String::new();
    let mut in_word = false;
    let mut word_start = start;
    let mut quote: Option<char> = None;
    let mut index = start;
    while index < end {
        let ch = command[index];
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    chars.push(ch);
                }
            }
            Some(_) => {
                if ch == '"' {
                    quote = None;
                } else if ch == '\\' && index + 1 < end {
                    index += 1;
                    chars.push(command[index]);
                } else {
                    chars.push(ch);
                }
            }
            None if matches!(ch, ' ' | '\t' | '\n') => {
                if in_word {
                    words.push(ShellWord {
                        text: std::mem::take(&mut chars),
                        start: word_start,
                        end: index,
                    });
                    if reach == Reach::FirstWord {
                        return words;
                    }
                    in_word = false;
                }
            }
            None if ch == '#' && !in_word => break,
            None => {
                if !in_word {
                    word_start = index;
                }
                in_word = true;
                let next = (index + 1 < end).then(|| command[index + 1]);
                if ch == '\'' || ch == '"' {
                    quote = Some(ch);
                } else if ch == '$' && next == Some('\'') {
                    let (text, after) = ansi_c_quoted_word(command, index + 1, end);
                    chars.push_str(&text);
                    index = after - 1;
                } else if ch == '$' && next == Some('"') {
                    quote = Some('"');
                    index += 1;
                } else if ch == '`' {
                    let mut stop = index + 1;
                    while stop < end {
                        if command[stop] == '\\' {
                            stop += 2;
                            continue;
                        }
                        stop += 1;
                        if command[stop - 1] == '`' {
                            break;
                        }
                    }
                    chars.extend(&command[index..stop.min(command.len())]);
                    index = stop - 1;
                } else if ch == '\\' && next.is_some() {
                    index += 1;
                    chars.push(command[index]);
                } else {
                    chars.push(ch);
                }
            }
        }
        index += 1;
    }
    if in_word {
        words.push(ShellWord {
            text: chars,
            start: word_start,
            end: index,
        });
    }
    words
}

/// The word a `$'...'` span builds and the index just past it. The escapes
/// are decoded and the `$` and quotes dropped; an escaped quote does not close
/// the span, and an unterminated span ends with the slice.
fn ansi_c_quoted_word(command: &[char], quote_start: usize, end: usize) -> (String, usize) {
    let mut text = String::new();
    let mut index = quote_start + 1;
    while index < end {
        let ch = command[index];
        if ch == '\'' {
            index += 1;
            break;
        }
        if ch == '\\' {
            let (decoded, after) = ansi_c_escape(command, index, end);
            text.push_str(&decoded);
            index = after;
            continue;
        }
        text.push(ch);
        index += 1;
    }
    (text, index)
}

/// Bash's single-character `$'...'` escapes.
fn simple_escape(ch: char) -> Option<char> {
    Some(match ch {
        'a' => '\u{7}',
        'b' => '\u{8}',
        'e' | 'E' => '\u{1b}',
        'f' => '\u{c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\u{b}',
        '\\' => '\\',
        '\'' => '\'',
        '"' => '"',
        '?' => '?',
        _ => return None,
    })
}

/// A decoded code point. A lone surrogate (which bash would emit as bytes)
/// is no character a word check can match, so it reads as U+FFFD.
fn code_point(value: u32) -> char {
    char::from_u32(value).unwrap_or('\u{fffd}')
}

/// Decode the escape at `command[index]` (a backslash): the text and the next
/// index. Known escapes, `\nnn`/`\0nnn` octal, `\xHH`, `\uXXXX`/`\UXXXXXXXX`
/// and `\cX` (the low five bits of X's code point) decode; an escape bash does
/// not know keeps its backslash (`$'\q'` is `\q`).
fn ansi_c_escape(command: &[char], index: usize, end: usize) -> (String, usize) {
    let Some(&following) = command.get(index + 1).filter(|_| index + 1 < end) else {
        return ("\\".to_string(), index + 1);
    };
    if let Some(simple) = simple_escape(following) {
        return (simple.to_string(), index + 2);
    }
    if following == 'c' && index + 2 < end {
        return (
            code_point(u32::from(command[index + 2]) & 0x1F).to_string(),
            index + 3,
        );
    }
    let hex_run = |from: usize, width: usize| {
        let mut cursor = from;
        while cursor < end && cursor - from < width && command[cursor].is_ascii_hexdigit() {
            cursor += 1;
        }
        let digits: String = command[from..cursor].iter().collect();
        (digits, cursor)
    };
    match following {
        '0'..='7' => {
            let mut cursor = index + 1;
            if command[cursor] == '0' {
                cursor += 1;
            }
            let from = cursor;
            while cursor < end && cursor - from < 3 && matches!(command[cursor], '0'..='7') {
                cursor += 1;
            }
            if cursor == from {
                return ("\\0".to_string(), cursor);
            }
            let digits: String = command[from..cursor].iter().collect();
            let value = u32::from_str_radix(&digits, 8).unwrap_or(0) & 0xFF;
            (code_point(value).to_string(), cursor)
        }
        'x' => {
            let (digits, cursor) = hex_run(index + 2, 2);
            if digits.is_empty() {
                return ("\\x".to_string(), index + 2);
            }
            let value = u32::from_str_radix(&digits, 16).unwrap_or(0);
            (code_point(value).to_string(), cursor)
        }
        'u' | 'U' => {
            let width = if following == 'u' { 4 } else { 8 };
            let (digits, cursor) = hex_run(index + 2, width);
            match u32::from_str_radix(&digits, 16) {
                Ok(value) if value <= 0x10_FFFF => (code_point(value).to_string(), cursor),
                Ok(_) | Err(_) => (format!("\\{following}"), index + 2),
            }
        }
        _ => (format!("\\{following}"), index + 2),
    }
}

/// The redirection operators a word may start with after its descriptor
/// digits, in the order the Python pattern tries them.
const DIGIT_OPERATORS: [&str; 8] = ["&>>", ">&", ">>", "<<", "<>", "<", ">", "&>"];
/// The operators a `{name}` descriptor takes (not the both-stream ones).
const BRACE_OPERATORS: [&str; 6] = [">&", ">>", "<<", "<>", "<", ">"];

fn starts_with_at(word: &[char], at: usize, prefix: &str) -> bool {
    (at..)
        .zip(prefix.chars())
        .all(|(position, expected)| word.get(position) == Some(&expected))
}

/// The length of a `{name}` descriptor at the start of `word` (`{fd}`), if any.
pub(super) fn brace_descriptor_len(word: &[char]) -> Option<usize> {
    if word.first() != Some(&'{') {
        return None;
    }
    let first = *word.get(1)?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let mut position = 2;
    while word
        .get(position)
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
    {
        position += 1;
    }
    (word.get(position) == Some(&'}')).then_some(position + 1)
}

/// How much of `word` a redirection word prefix covers (`2>`, `>&`, `&>>`,
/// `{fd}>`), or `None` when the word is not one: such words change where
/// output goes, never what is printed.
pub(super) fn redirect_prefix_len(word: &[char]) -> Option<usize> {
    let digits = word.iter().take_while(|ch| is_digit(**ch)).count();
    if let Some(operator) = DIGIT_OPERATORS
        .iter()
        .find(|operator| starts_with_at(word, digits, operator))
    {
        return Some(digits + operator.len());
    }
    let brace = brace_descriptor_len(word)?;
    BRACE_OPERATORS
        .iter()
        .find(|operator| starts_with_at(word, brace, operator))
        .map(|operator| brace + operator.len())
}

/// `(command word, redirection word)` for a word with a glued redirection
/// (`env>&2` is env sending its output to fd 2). An operator-first word
/// (`2>&1`, `&>log`) and an all-digit head (`12>file`) stay whole.
fn split_glued_redirect(word: &str) -> (String, Option<String>) {
    let chars: Vec<char> = word.chars().collect();
    if redirect_prefix_len(&chars).is_some() {
        return (word.to_string(), None);
    }
    for index in 1..chars.len() {
        let ch = chars[index];
        if ch == '>' || ch == '<' || (ch == '&' && chars.get(index + 1) == Some(&'>')) {
            let head: String = chars[..index].iter().collect();
            if all_digits(&head) {
                return (word.to_string(), None);
            }
            return (head, Some(chars[index..].iter().collect()));
        }
    }
    (word.to_string(), None)
}

/// A POSIX `FOO=1` prefix word.
pub(super) fn is_assignment_word(word: &str) -> bool {
    let mut chars = word.chars();
    if !chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
    {
        return false;
    }
    for ch in chars {
        if ch == '=' {
            return true;
        }
        if !(ch.is_ascii_alphanumeric() || ch == '_') {
            return false;
        }
    }
    false
}

/// Shell words for one segment, minus the words that narrow nothing:
/// redirection words (a bare operator also swallows its target word; a glued
/// one is split off its command word first), then leading `FOO=1`
/// assignments, since the shell runs the rest of the command either way.
pub(super) fn analysis_words(command: &[char], start: usize, end: usize) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut skip_target = false;
    for word in shell_words(command, start, end, Reach::All) {
        if skip_target {
            skip_target = false;
            continue;
        }
        let (head, redirect) = split_glued_redirect(&word.text);
        let pieces = match redirect {
            None => vec![word.text],
            Some(redirect) => vec![head, redirect],
        };
        for piece in pieces {
            let chars: Vec<char> = piece.chars().collect();
            if let Some(covered) = redirect_prefix_len(&chars) {
                skip_target = covered == chars.len();
                continue;
            }
            words.push(piece);
        }
    }
    let assignments = words
        .iter()
        .take_while(|word| is_assignment_word(word))
        .count();
    words.drain(..assignments);
    words
}
