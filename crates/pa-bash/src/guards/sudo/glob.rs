//! Does a glob pattern in a command word match the name `sudo` or `doas`?
//!
//! The pattern is translated the way the guard always translated it (`*` to
//! `.*`, `?` to `.`, a bracket expression to a character class with POSIX
//! classes such as `[:lower:]` expanded to their ranges, every other
//! character escaped) and that regular expression is read with Python `re`
//! semantics: a malformed class (a bad range, an unterminated set) matches
//! nothing, and a bracket body's raw characters keep their regex meaning.

use super::tables::SUDO_COMMAND_WORDS;

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
    let Some(regex) = Regex::compile(&glob_regex(value)) else {
        return false;
    };
    SUDO_COMMAND_WORDS.iter().any(|name| regex.full_match(name))
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
    fn matches(self, ch: char) -> bool {
        match self {
            Category::Digit => ch.is_numeric(),
            Category::NotDigit => !ch.is_numeric(),
            Category::Space => ch.is_whitespace(),
            Category::NotSpace => !ch.is_whitespace(),
            Category::Word => ch.is_alphanumeric() || ch == '_',
            Category::NotWord => !(ch.is_alphanumeric() || ch == '_'),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ClassItem {
    Range(u32, u32),
    Category(Category),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Atom {
    Literal(char),
    AnyButNewline,
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

impl Atom {
    fn matches(&self, ch: char) -> bool {
        match self {
            Atom::Literal(literal) => *literal == ch,
            Atom::AnyButNewline => ch != '\n',
            Atom::Class { negated, items } => {
                let code = u32::from(ch);
                let hit = items.iter().any(|item| match item {
                    ClassItem::Range(low, high) => (*low..=*high).contains(&code),
                    ClassItem::Category(category) => category.matches(ch),
                });
                hit != *negated
            }
        }
    }
}

/// The subset of Python `re` the translated globs use: literals and escapes,
/// `.`, `*`, and character classes.
struct Regex {
    items: Vec<(Atom, bool)>,
}

/// A class escape's meaning: one code point or a category.
#[derive(Clone, Copy)]
enum Escaped {
    Code(u32),
    Category(Category),
}

impl Regex {
    fn compile(pattern: &str) -> Option<Self> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut items: Vec<(Atom, bool)> = Vec::new();
        let mut index = 0;
        while index < chars.len() {
            match chars[index] {
                '*' => {
                    let last = items.last_mut()?;
                    if last.1 {
                        return None; // multiple repeat
                    }
                    last.1 = true;
                    index += 1;
                }
                '.' => {
                    items.push((Atom::AnyButNewline, false));
                    index += 1;
                }
                '[' => {
                    let (atom, next) = parse_class(&chars, index + 1)?;
                    items.push((atom, false));
                    index = next;
                }
                '\\' => {
                    let escaped = *chars.get(index + 1)?;
                    if escaped.is_ascii_alphanumeric() {
                        return None; // not an escape the translation produces
                    }
                    items.push((Atom::Literal(escaped), false));
                    index += 2;
                }
                '(' | ')' | '+' | '?' | '{' | '|' | '^' | '$' => return None,
                literal => {
                    items.push((Atom::Literal(literal), false));
                    index += 1;
                }
            }
        }
        Some(Self { items })
    }

    fn full_match(&self, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        match_from(&self.items, &chars)
    }
}

fn match_from(items: &[(Atom, bool)], text: &[char]) -> bool {
    let Some(((atom, star), rest)) = items.split_first() else {
        return text.is_empty();
    };
    if *star {
        let mut taken = 0;
        loop {
            if match_from(rest, &text[taken..]) {
                return true;
            }
            if taken < text.len() && atom.matches(text[taken]) {
                taken += 1;
            } else {
                return false;
            }
        }
    }
    text.first().is_some_and(|&ch| atom.matches(ch)) && match_from(rest, &text[1..])
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

/// Parse a class from just after its `[`: the atom and the index after `]`.
fn parse_class(chars: &[char], mut index: usize) -> Option<(Atom, usize)> {
    let negated = chars.get(index) == Some(&'^');
    if negated {
        index += 1;
    }
    let mut items: Vec<ClassItem> = Vec::new();
    loop {
        let this = *chars.get(index)?;
        if this == ']' && !items.is_empty() {
            return Some((Atom::Class { negated, items }, index + 1));
        }
        let (first, next) = if this == '\\' {
            class_escape(chars, index)?
        } else {
            (Escaped::Code(u32::from(this)), index + 1)
        };
        index = next;
        if chars.get(index) == Some(&'-') {
            index += 1;
            let that = *chars.get(index)?;
            if that == ']' {
                items.push(single(first));
                items.push(ClassItem::Range(u32::from('-'), u32::from('-')));
                return Some((Atom::Class { negated, items }, index + 1));
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
            items.push(ClassItem::Range(low, high));
        } else {
            items.push(single(first));
        }
    }
}

fn single(escaped: Escaped) -> ClassItem {
    match escaped {
        Escaped::Code(code) => ClassItem::Range(code, code),
        Escaped::Category(category) => ClassItem::Category(category),
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
