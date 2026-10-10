//! Evidence words: the visible text a rule needs before it may refuse code
//! it cannot read.

/// Whether `text` holds `word` as a whole word (ASCII case-insensitive; a
/// word boundary is anything but `[A-Za-z0-9_]`), and where.
pub(crate) fn find_word(text: &str, word: &str) -> Option<usize> {
    let haystack = text.as_bytes();
    let needle = word.as_bytes();
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    (0..=haystack.len() - needle.len()).find(|&start| {
        haystack[start..start + needle.len()].eq_ignore_ascii_case(needle)
            && (start == 0 || !is_word(haystack[start - 1]))
            && haystack
                .get(start + needle.len())
                .is_none_or(|&byte| !is_word(byte))
    })
}

/// The first of `words` that `text` holds as a whole word.
pub(crate) fn first_word<'w>(text: &str, words: &[&'w str]) -> Option<&'w str> {
    words
        .iter()
        .copied()
        .find(|word| find_word(text, word).is_some())
}

/// Whether `text` holds `word` as a separate shell word (delimited by
/// blanks, operators, quotes or the text edges) in a command position:
/// at the start, after an operator or an opening `(`, `{` or backquote,
/// or after a keyword or wrapper that runs the next word. So `env` matches
/// `env | sort` and `x; "$(env)"` but not `.env`, `env.sh` or `echo env`.
pub(crate) fn find_command_word(text: &str, word: &str) -> bool {
    const RUNS_NEXT: [&str; 14] = [
        "then", "do", "else", "if", "elif", "while", "until", "!", "time", "exec", "command",
        "nohup", "sudo", "doas",
    ];
    let is_edge =
        |ch: Option<char>| ch.is_none_or(|ch| ch.is_whitespace() || "|&;()<>`'\"$".contains(ch));
    let mut start = 0;
    while let Some(found) = text[start..].find(word) {
        let at = start + found;
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        if is_edge(before) && is_edge(after) {
            // Step back over the quotes that may open the word.
            let head = text[..at]
                .trim_end_matches(['"', '\''])
                .trim_end_matches([' ', '\t']);
            let previous = head
                .split(|ch: char| ch.is_whitespace() || "|&;(`{".contains(ch))
                .next_back()
                .unwrap_or_default();
            let opener = head
                .chars()
                .next_back()
                .is_none_or(|ch| "|&;(`{\n".contains(ch));
            if opener || RUNS_NEXT.contains(&previous) {
                return true;
            }
        }
        start = at + word.len();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_words_only() {
        assert!(find_word("git push origin", "push").is_some());
        assert!(find_word("pushd x", "push").is_none());
        assert!(find_word("on:\n  push:", "push").is_some());
        assert!(find_word("SUDO id", "sudo").is_some());
        assert_eq!(first_word("chown -R x", &["chmod", "chown"]), Some("chown"));
        assert!(find_command_word("x; env | sort", "env"));
        assert!(!find_command_word(". env.sh; cat .env", "env"));
        assert!(find_command_word("echo \"$(echo \"$(env)\")\"", "env"));
        assert!(find_command_word("if true; then env; fi", "env"));
        assert!(!find_command_word("echo \"$(echo env)\"", "env"));
        assert!(!find_command_word("cat <<EOF\nprint env\nEOF", "env"));
    }
}
