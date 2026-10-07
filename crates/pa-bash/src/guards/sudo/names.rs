//! Whether a command word can name sudo or doas: its basename, folded case,
//! brace-expansion alternatives, glob patterns, and the surviving letters of a
//! word built from quoting or expansion.

use super::glob::matches_sudo_pattern;
use super::tables::SUDO_COMMAND_WORDS;

/// How many brace alternatives (and groups) the scan enumerates before it
/// fails closed.
const BRACE_EXPANSION_CAP: usize = 64;

/// `os.path.basename` (POSIX): the text after the last `/`.
pub(super) fn basename(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

/// A plausible program name: letters, digits, and the punctuation real
/// executable names use (`[A-Za-z0-9][A-Za-z0-9._+-]*`). Such a word is judged
/// by its basename alone, so `sudoku` and `sudo-report` stay runnable.
fn is_plain_command_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-'))
}

/// True when a command word can name sudo/doas: braces, globs, and letters.
///
/// Every test reads the basename, the name the shell resolves. A plain
/// program name is judged by that basename alone, while globs and every word
/// that carries quoting or expansion fall back to their surviving letters and
/// fail closed.
pub(super) fn word_names_sudo(value: &str) -> bool {
    let Some(alternatives) = brace_alternatives(value) else {
        return true; // too many alternatives to enumerate: fail closed
    };
    alternatives.iter().any(|candidate| {
        let name = basename(candidate);
        // Folded case: a case-insensitive filesystem resolves `SUDO` to sudo.
        if SUDO_COMMAND_WORDS.contains(&name.to_lowercase().as_str()) {
            return true;
        }
        if matches_sudo_pattern(name) {
            return true;
        }
        let letters: String = candidate
            .chars()
            .filter(|ch| ch.is_alphabetic())
            .collect::<String>()
            .to_lowercase();
        (letters.contains("sudo") || letters.contains("doas")) && !is_plain_command_name(name)
    })
}

/// Python's `str.isdigit` over a whole (non-empty) string after any leading
/// dashes are dropped.
fn is_integer(value: &str) -> bool {
    let digits = value.trim_start_matches('-');
    !digits.is_empty() && digits.chars().all(char::is_numeric)
}

/// Python's `int(text)` for the strings [`is_integer`] admits: `None` where
/// Python raises (`--5`) or the value exceeds the scan's integer range (both
/// fail closed the same way).
fn parse_integer(text: &str) -> Option<i128> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let value: i128 = digits.parse().ok()?;
    Some(if negative { -value } else { value })
}

/// One brace group's expansion: how many elements it has, and the elements
/// when that count is within the cap (`None` past it, so the caller fails
/// closed without building them).
type GroupElements = (usize, Option<Vec<String>>);

/// Elements of an `x..y` / `x..y..step` brace sequence, else `None` (the
/// group is not a sequence). The count is computed arithmetically first, so
/// an oversized range never builds its elements.
fn sequence_elements(body: &str) -> Option<GroupElements> {
    let parts: Vec<&str> = body.split("..").collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let (start_text, end_text) = (parts[0], parts[1]);
    let step_text = parts.get(2).map_or("1", |step| super::lexer::strip(step));
    if !is_integer(step_text) {
        return None;
    }
    let numeric = is_integer(start_text) && is_integer(end_text);
    let single_chars = start_text.chars().count() == 1 && end_text.chars().count() == 1;
    if !numeric && !single_chars {
        return None;
    }
    let Some(step) = parse_integer(step_text).map(i128::abs) else {
        return Some((0, None));
    };
    if step == 0 {
        return None; // `{1..9..0}` does not expand in bash
    }
    let bounds = if numeric {
        parse_integer(start_text).zip(parse_integer(end_text))
    } else {
        let code = |text: &str| text.chars().next().map(|ch| i128::from(u32::from(ch)));
        code(start_text).zip(code(end_text))
    };
    let Some((low, high)) = bounds else {
        return Some((0, None));
    };
    let step = if low > high { -step } else { step };
    let Some(count) = high
        .checked_sub(low)
        // Span and step share a sign, so truncating division is Python's floor.
        .map(|span| span / step + 1)
        .and_then(|count| usize::try_from(count).ok())
    else {
        return Some((0, None));
    };
    if count > BRACE_EXPANSION_CAP {
        return Some((count, None));
    }
    let elements = (0..count)
        .map(|offset| low + step * i128::try_from(offset).unwrap_or(0))
        .map(|value| {
            if numeric {
                value.to_string()
            } else {
                u32::try_from(value)
                    .ok()
                    .and_then(char::from_u32)
                    .unwrap_or('\u{fffd}')
                    .to_string()
            }
        })
        .collect();
    Some((count, Some(elements)))
}

/// Split a brace body on its top-level commas, or `None` past the cap.
fn top_level_split(body: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth: i64 = 0;
    for ch in body.chars() {
        match ch {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
        if ch == ',' && depth == 0 {
            parts.push(std::mem::take(&mut current));
            if parts.len() > BRACE_EXPANSION_CAP {
                return None; // stop early instead of materialising a huge group
            }
            continue;
        }
        current.push(ch);
    }
    parts.push(current);
    Some(parts)
}

/// Elements of an expanding brace group, or `None` when it stays literal
/// (`su{d}o` has no comma, so bash does not expand it).
fn brace_group_elements(body: &str) -> Option<GroupElements> {
    if let Some(sequence) = sequence_elements(body) {
        return Some(sequence);
    }
    let Some(parts) = top_level_split(body) else {
        return Some((0, None));
    };
    if parts.len() < 2 {
        return None;
    }
    Some((parts.len(), Some(parts)))
}

/// Leftmost expanding brace group as (prefix, elements, suffix). One pass
/// with a stack matches every `{` to its `}` once, so brace-heavy words stay
/// linear.
fn first_brace_group(chars: &[char]) -> Option<(String, GroupElements, Vec<char>)> {
    let mut open = Vec::new();
    let mut groups = Vec::new();
    for (index, &ch) in chars.iter().enumerate() {
        if ch == '{' {
            open.push(index);
        } else if ch == '}' {
            if let Some(start) = open.pop() {
                groups.push((start, index));
            }
        }
    }
    groups.sort_unstable();
    groups.into_iter().find_map(|(start, end)| {
        let body: String = chars[start + 1..end].iter().collect();
        brace_group_elements(&body).map(|elements| {
            (
                chars[..start].iter().collect(),
                elements,
                chars[end + 1..].to_vec(),
            )
        })
    })
}

/// Brace-expansion candidates of a word, or `None` when they exceed the cap.
fn brace_alternatives(value: &str) -> Option<Vec<String>> {
    if value.matches('{').count() > BRACE_EXPANSION_CAP {
        return None; // a brace flood: more groups than the cap enumerates
    }
    let mut groups = Vec::new();
    let mut tail: Vec<char> = value.chars().collect();
    while let Some((prefix, elements, suffix)) = first_brace_group(&tail) {
        groups.push((prefix, elements));
        tail = suffix;
    }
    if groups.is_empty() {
        return Some(vec![value.to_string()]);
    }
    let mut total = 1usize;
    for (_, (count, elements)) in &groups {
        if elements.is_none() || *count > BRACE_EXPANSION_CAP {
            return None;
        }
        total = total.saturating_mul(*count);
        if total > BRACE_EXPANSION_CAP {
            return None;
        }
    }
    let mut expanded = vec![String::new()];
    for (prefix, (_, elements)) in &groups {
        let elements = elements.as_deref().unwrap_or_default();
        expanded = expanded
            .iter()
            .flat_map(|candidate| {
                elements
                    .iter()
                    .map(move |element| format!("{candidate}{prefix}{element}"))
            })
            .collect();
    }
    let tail: String = tail.into_iter().collect();
    Some(
        expanded
            .into_iter()
            .map(|candidate| candidate + &tail)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brace_alternatives_expand_sequences_and_lists() {
        assert_eq!(
            brace_alternatives("su{d,}o"),
            Some(vec!["sudo".into(), "suo".into()])
        );
        assert_eq!(brace_alternatives("s{u..u}do"), Some(vec!["sudo".into()]));
        assert_eq!(brace_alternatives("su{d}o"), Some(vec!["su{d}o".into()]));
        assert_eq!(brace_alternatives(&"{a,b}".repeat(22)), None);
        assert_eq!(brace_alternatives("{1..9999999}"), None);
    }

    #[test]
    fn plain_program_names_are_judged_by_basename() {
        assert!(word_names_sudo("/usr/bin/sudo"));
        assert!(word_names_sudo("SUDO"));
        assert!(!word_names_sudo("sudoku"));
        assert!(!word_names_sudo("/usr/bin/sudo-report"));
        assert!(word_names_sudo("${SUDO_CMD:-sudo}"));
    }
}
