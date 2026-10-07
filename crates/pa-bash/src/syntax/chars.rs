//! The character classes the guards were specified in (Python `str`
//! semantics), and Python-style slicing of char buffers.

/// Python `str.isspace()`: Unicode whitespace plus the ASCII information
/// separators `\x1c`-`\x1f` (also what `\s` matches in a Python `str`
/// pattern).
pub(crate) fn is_space(ch: char) -> bool {
    ch.is_whitespace() || ('\x1c'..='\x1f').contains(&ch)
}

/// `[A-Za-z0-9_]`.
pub(crate) fn is_ascii_word(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

/// `text[start:end]` with Python's clamping, as a `String`.
pub(crate) fn py_slice(text: &[char], start: usize, end: usize) -> String {
    let end = end.min(text.len());
    let start = start.min(end);
    text[start..end].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_follow_python() {
        assert!(is_space('\u{1f}') && is_space('\u{a0}') && !is_space('x'));
        assert_eq!(py_slice(&['a', 'b', 'c'], 2, 9), "c");
        assert_eq!(py_slice(&['a', 'b', 'c'], 5, 9), "");
    }
}
