//! Does a glob pattern in a command word match the name `sudo` or `doas`?
//!
//! The pattern is translated the way the guard always translated it (`*` to
//! `.*`, `?` to `.`, a bracket expression to a character class with POSIX
//! classes such as `[:lower:]` expanded to their ranges, every other
//! character escaped) and that regular expression is read with Python `re`
//! semantics: a malformed class (a bad range, an unterminated set) matches
//! nothing, and a bracket body's raw characters keep their regex meaning.

use super::tables::SUDO_COMMAND_WORDS;
use crate::syntax::pyre::PyRegex;

/// POSIX character classes as the regex ranges bash matches; a class the
/// table does not name still matches one character in bash, so it becomes
/// `.` (refusing too much is the fail-closed direction).
fn posix_class_ranges(name: &str) -> &'static str {
    match name {
        "alnum" => "a-zA-Z0-9",
        "alpha" => "a-zA-Z",
        "ascii" => "\\x00-\\x7f",
        "blank" => " \\t",
        "cntrl" => "\\x00-\\x1f\\x7f",
        "digit" => "0-9",
        "graph" => "!-~",
        "lower" => "a-z",
        "print" => " -~",
        "punct" => "!-/:-@\\[-`{-~",
        "space" => " \\t\\r\\n\\v\\f",
        "upper" => "A-Z",
        "word" => "a-zA-Z0-9_",
        "xdigit" => "0-9A-Fa-f",
        _ => ".",
    }
}

fn starts_with_at(chars: &[char], index: usize, needle: &str) -> bool {
    let needle: Vec<char> = needle.chars().collect();
    chars.get(index..index + needle.len()) == Some(needle.as_slice())
}

fn find_at(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    (from..chars.len()).find(|&index| starts_with_at(chars, index, needle))
}

/// Index of the `]` closing the bracket expression at `start`. A POSIX class
/// nests its own brackets, and a `]` in the first position is a literal.
fn bracket_end(chars: &[char], start: usize) -> Option<usize> {
    let mut index = start + 1;
    if matches!(chars.get(index), Some('!' | '^')) {
        index += 1;
    }
    if chars.get(index) == Some(&']') {
        index += 1;
    }
    while index < chars.len() {
        if starts_with_at(chars, index, "[:") {
            if let Some(close) = find_at(chars, index + 2, ":]") {
                index = close + 2;
                continue;
            }
        }
        if chars[index] == ']' {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Regex text for a bracket body, POSIX classes expanded.
fn bracket_body(body: &[char]) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index < body.len() {
        if starts_with_at(body, index, "[:") {
            if let Some(close) = find_at(body, index + 2, ":]") {
                let name: String = body[index + 2..close].iter().collect();
                out.push_str(posix_class_ranges(&name));
                index = close + 2;
                continue;
            }
        }
        out.push(body[index]);
        index += 1;
    }
    out
}

/// Python `re.escape` for one character.
fn push_escaped(out: &mut String, ch: char) {
    if "()[]{}?*+-|^$\\.&~# \t\n\r\u{b}\u{c}".contains(ch) {
        out.push('\\');
    }
    out.push(ch);
}

/// The regular expression the guard reads a glob word as.
fn glob_regex(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut pattern = String::new();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '*' => pattern.push_str(".*"),
            '?' => pattern.push('.'),
            '[' => match bracket_end(&chars, index) {
                None => pattern.push_str("\\["),
                Some(end) => {
                    // Negation is the literal first character of the body,
                    // tested before classes expand (`[:graph:]` starts with `!`).
                    let negated = chars[index + 1] == '!';
                    let body_start = if negated { index + 2 } else { index + 1 };
                    pattern.push('[');
                    if negated {
                        pattern.push('^');
                    }
                    pattern.push_str(&bracket_body(&chars[body_start..end]));
                    pattern.push(']');
                    index = end;
                }
            },
            other => push_escaped(&mut pattern, other),
        }
        index += 1;
    }
    pattern
}

/// True when the glob pattern in a word matches the name sudo or doas.
pub(super) fn matches_sudo_pattern(value: &str) -> bool {
    if !value.contains(['*', '?', '[']) {
        return false;
    }
    let Some(regex) = python_regex(&glob_regex(value)) else {
        return false;
    };
    SUDO_COMMAND_WORDS
        .iter()
        .any(|name| regex.is_full_match(*name))
}

/// A `\d`-style category inside a class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    Digit,
    NotDigit,
    Space,
    NotSpace,
    Word,
    NotWord,
}

impl Category {
    fn regex(self) -> &'static str {
        match self {
            Category::Digit => r"\d",
            Category::NotDigit => r"\D",
            Category::Space => r"\s",
            Category::NotSpace => r"\S",
            Category::Word => r"\w",
            Category::NotWord => r"\W",
        }
    }
}

/// A class escape's meaning: one code point or a category.
#[derive(Clone, Copy)]
enum Escaped {
    Code(u32),
    Category(Category),
}

/// The translated glob, read by Python's `re.compile` rules (a malformed
/// class, a bad range, or a stray repeat is an error: `None`), re-spelled in
/// the `fancy-regex` dialect with every literal as a `\x{..}` code point so
/// no character keeps a meaning the two dialects disagree on.
fn python_regex(pattern: &str) -> Option<PyRegex> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut repeatable = false;
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '*' => {
                if !repeatable {
                    return None; // nothing to repeat, or a multiple repeat
                }
                out.push('*');
                repeatable = false;
                index += 1;
                continue;
            }
            '.' => {
                out.push('.');
                index += 1;
            }
            '[' => {
                index = class(&chars, index + 1, &mut out)?;
            }
            '\\' => {
                let escaped = *chars.get(index + 1)?;
                if escaped.is_ascii_alphanumeric() {
                    return None; // not an escape the translation produces
                }
                push_code(&mut out, u32::from(escaped));
                index += 2;
            }
            '(' | ')' | '+' | '?' | '{' | '|' | '^' | '$' => return None,
            literal => {
                push_code(&mut out, u32::from(literal));
                index += 1;
            }
        }
        repeatable = true;
    }
    PyRegex::compile(&out).ok()
}

/// One code point as `\x{..}`. Python accepts a lone surrogate in a pattern
/// where the regex crate does not; only `sudo`/`doas` are ever matched, so a
/// surrogate stands in as U+10FFFF, which no ASCII name contains either.
fn push_code(out: &mut String, code: u32) {
    use std::fmt::Write as _;
    let code = if (0xD800..=0xDFFF).contains(&code) {
        0x0010_FFFF
    } else {
        code
    };
    let _ = write!(out, "\\x{{{code:x}}}");
}

/// One class escape (`\x41`, `\t`, `\d`, `\[`), Python `_class_escape`.
fn class_escape(chars: &[char], index: usize) -> Option<(Escaped, usize)> {
    let code = *chars.get(index + 1)?;
    let simple = match code {
        'a' => Some(7),
        'b' => Some(8),
        'f' => Some(12),
        'n' => Some(10),
        'r' => Some(13),
        't' => Some(9),
        'v' => Some(11),
        '\\' => Some(92),
        _ => None,
    };
    if let Some(simple) = simple {
        return Some((Escaped::Code(simple), index + 2));
    }
    let category = match code {
        'd' => Some(Category::Digit),
        'D' => Some(Category::NotDigit),
        's' => Some(Category::Space),
        'S' => Some(Category::NotSpace),
        'w' => Some(Category::Word),
        'W' => Some(Category::NotWord),
        _ => None,
    };
    if let Some(category) = category {
        return Some((Escaped::Category(category), index + 2));
    }
    let hex = |from: usize, width: usize| -> Option<u32> {
        let digits = chars.get(from..from + width)?;
        digits.iter().all(char::is_ascii_hexdigit).then(|| {
            digits
                .iter()
                .fold(0, |acc, d| acc * 16 + d.to_digit(16).unwrap_or(0))
        })
    };
    match code {
        'x' => hex(index + 2, 2).map(|value| (Escaped::Code(value), index + 4)),
        'u' => hex(index + 2, 4).map(|value| (Escaped::Code(value), index + 6)),
        'U' => hex(index + 2, 8)
            .filter(|value| *value <= 0x0010_FFFF)
            .map(|value| (Escaped::Code(value), index + 10)),
        '0'..='7' => {
            let mut cursor = index + 2;
            let mut value = code.to_digit(8).unwrap_or(0);
            while cursor < index + 4 && chars.get(cursor).is_some_and(|ch| ('0'..='7').contains(ch))
            {
                value = value * 8 + chars[cursor].to_digit(8).unwrap_or(0);
                cursor += 1;
            }
            (value <= 0o377).then_some((Escaped::Code(value), cursor))
        }
        other if other.is_ascii_alphanumeric() => None,
        other => Some((Escaped::Code(u32::from(other)), index + 2)),
    }
}

/// Parse a Python class from just after its `[`, write it to `out`, and
/// return the index after its `]`.
fn class(chars: &[char], mut index: usize, out: &mut String) -> Option<usize> {
    out.push('[');
    if chars.get(index) == Some(&'^') {
        out.push('^');
        index += 1;
    }
    let mut empty = true;
    loop {
        let this = *chars.get(index)?;
        if this == ']' && !empty {
            out.push(']');
            return Some(index + 1);
        }
        empty = false;
        let (first, next) = if this == '\\' {
            class_escape(chars, index)?
        } else {
            (Escaped::Code(u32::from(this)), index + 1)
        };
        index = next;
        if chars.get(index) != Some(&'-') {
            push_item(out, first);
            continue;
        }
        index += 1;
        let that = *chars.get(index)?;
        if that == ']' {
            push_item(out, first);
            push_code(out, u32::from('-'));
            out.push(']');
            return Some(index + 1);
        }
        let (second, next) = if that == '\\' {
            class_escape(chars, index)?
        } else {
            (Escaped::Code(u32::from(that)), index + 1)
        };
        index = next;
        let (Escaped::Code(low), Escaped::Code(high)) = (first, second) else {
            return None; // bad character range
        };
        if high < low {
            return None;
        }
        // A range is a set of scalar values: surrogate endpoints move inward
        // (a range of surrogates only holds nothing an ASCII name has).
        let low = if (0xD800..=0xDFFF).contains(&low) {
            0xE000
        } else {
            low
        };
        let high = if (0xD800..=0xDFFF).contains(&high) {
            0xD7FF
        } else {
            high
        };
        if low > high {
            push_code(out, 0x0010_FFFF);
            continue;
        }
        push_code(out, low);
        out.push('-');
        push_code(out, high);
    }
}

fn push_item(out: &mut String, item: Escaped) {
    match item {
        Escaped::Code(code) => push_code(out, code),
        Escaped::Category(category) => out.push_str(category.regex()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_and_posix_classes_match_sudo() {
        for word in [
            "sud[o]",
            "[s]udo",
            "su?do",
            "sud?",
            "/usr/bin/su*",
            "[[:lower:]]udo",
            "[[:graph:]]udo",
            "[[:lower:]]oas",
        ] {
            let name = word.rsplit('/').next().unwrap_or(word);
            assert_eq!((word, matches_sudo_pattern(name)), (word, word != "su?do"));
        }
        assert!(!matches_sudo_pattern("s[[:upper:]]do"));
        assert!(!matches_sudo_pattern("[[:punct:]]udo"));
        assert!(!matches_sudo_pattern("[z-a]udo"));
    }
}
