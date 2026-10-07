//! Character classes and the small text patterns the scan matches.
//!
//! The scan indexes text by character (as the shell reads it), so every helper
//! here works on `&[char]` or `&str` by character. The classes follow Python's
//! `str` semantics the guard was specified in: `\s` is Unicode whitespace
//! (including the information separators U+001C..U+001F) and `\w` is a Unicode
//! letter or digit, or `_`.

/// Python's `str.isspace()` / regex `\s`.
pub(super) fn is_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

/// Python's regex `\w` for `str` patterns.
fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// `[A-Za-z0-9_]`.
pub(super) fn is_ascii_word(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

pub(super) fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

/// `text[start:end]` with Python's clamping.
pub(super) fn slice(text: &[char], start: usize, end: usize) -> String {
    let end = end.min(text.len());
    let start = start.min(end);
    text[start..end].iter().collect()
}

/// `str.casefold()` for the names the guard compares against (all ASCII):
/// a character folds into ASCII only through these spellings.
pub(super) fn casefold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            'ß' => out.push_str("ss"),
            'ſ' => out.push('s'),
            _ => out.extend(ch.to_lowercase()),
        }
    }
    out
}

/// A command word's name, folded for the filesystems the kernel runs on
/// (`os.path.basename(value).casefold()`).
pub(super) fn command_name(value: &str) -> String {
    casefold(value.rsplit('/').next().unwrap_or(value))
}

/// Whether `text` holds `name` as a whole word (`\bname\b`), optionally
/// ignoring ASCII case.
pub(super) fn has_word(text: &str, names: &[&str], ignore_case: bool) -> bool {
    let text = chars(text);
    let n = text.len();
    for start in 0..n {
        if start > 0 && is_word(text[start - 1]) {
            continue;
        }
        for name in names {
            let name = chars(name);
            let end = start + name.len();
            if end > n {
                continue;
            }
            let matched = text[start..end].iter().zip(&name).all(|(a, b)| {
                if ignore_case {
                    a.eq_ignore_ascii_case(b)
                } else {
                    a == b
                }
            });
            if matched && (end == n || !is_word(text[end])) {
                return true;
            }
        }
    }
    false
}

/// `(?<![A-Za-z0-9_])push(?![A-Za-z0-9_])`.
fn has_push_word(text: &[char]) -> bool {
    let name = ['p', 'u', 's', 'h'];
    (0..text.len()).any(|start| {
        (start == 0 || !is_ascii_word(text[start - 1]))
            && text[start..].starts_with(&name)
            && text.get(start + 4).is_none_or(|ch| !is_ascii_word(*ch))
    })
}

/// `[A-Za-z0-9_-]`: what may not follow a force flag.
fn is_flag_char(ch: char) -> bool {
    is_ascii_word(ch) || ch == '-'
}

/// The force signal of `_FP_FORCE_IN_TEXT`: `--force` (or `--forc`),
/// `--mirror` and its abbreviations, a short cluster ending in `f`, or a
/// `+`-prefixed word.
fn has_force_signal(text: &[char]) -> bool {
    let n = text.len();
    let ends_flag = |at: usize| at >= n || !is_flag_char(text[at]);
    for start in 0..n {
        if start > 0 && is_ascii_word(text[start - 1]) {
            continue;
        }
        let rest = &text[start..];
        for long in ["--force", "--forc", "--mirror", "--mirr", "--mir", "--m"] {
            let long: Vec<char> = long.chars().collect();
            if rest.starts_with(&long) && ends_flag(start + long.len()) {
                return true;
            }
        }
        if rest.first() == Some(&'-') {
            let run = rest[1..]
                .iter()
                .take_while(|ch| ch.is_ascii_alphabetic())
                .count();
            if run > 0 && rest[run] == 'f' && ends_flag(start + 1 + run) {
                return true;
            }
        }
        if rest.first() == Some(&'+')
            && rest
                .get(1)
                .is_some_and(|ch| !is_space(*ch) && !";&|()".contains(*ch))
        {
            return true;
        }
    }
    false
}

/// True when the text carries `push` together with a force signal.
pub(super) fn force_push_pattern_in_text(text: &str) -> bool {
    let text = chars(text);
    has_push_word(&text) && has_force_signal(&text)
}

/// The shells that run a script: the name list of `_FP_SHELL_C_INTERPRETERS`.
pub(super) const SHELL_NAMES: [&str; 8] =
    ["sh", "bash", "zsh", "dash", "ksh", "fish", "tcsh", "csh"];

/// `_FP_SHELL_INTERPRETER_IN_TEXT`: a shell name (case-insensitive, optional
/// `.exe`) not glued to a `[A-Za-z0-9_.-]` character on either side, so
/// `/bin/sh` and `./sh` count while `payload.sh` does not.
pub(super) fn has_shell_interpreter(text: &str) -> bool {
    let text = chars(text);
    let n = text.len();
    let glue = |ch: char| is_ascii_word(ch) || ch == '.' || ch == '-';
    for start in 0..n {
        if start > 0 && glue(text[start - 1]) {
            continue;
        }
        for name in SHELL_NAMES {
            let name: Vec<char> = name.chars().collect();
            let end = start + name.len();
            if end > n
                || !text[start..end]
                    .iter()
                    .zip(&name)
                    .all(|(a, b)| a.eq_ignore_ascii_case(b))
            {
                continue;
            }
            let exe = ['.', 'e', 'x', 'e'];
            let with_exe = end + 4 <= n
                && text[end..end + 4]
                    .iter()
                    .zip(&exe)
                    .all(|(a, b)| a.eq_ignore_ascii_case(b));
            if with_exe && text.get(end + 4).is_none_or(|ch| !glue(*ch)) {
                return true;
            }
            if text.get(end).is_none_or(|ch| !glue(*ch)) {
                return true;
            }
        }
    }
    false
}

/// `_FP_GLOB_OR_SUBSTITUTION`: substitution, globs, and brace expansion.
pub(super) fn has_glob_or_substitution(text: &str) -> bool {
    text.chars().any(|ch| "$`*?{}[]".contains(ch))
}

/// `_FP_DYNAMIC_COMMAND_WORD` / `_FP_PAYLOAD_EXPANSION`: a `$` or backtick.
pub(super) fn has_expansion(text: &str) -> bool {
    text.contains(['$', '`'])
}

/// `_FP_BRACE_EXPANSION`: `{...}` with a `,` or `..` and no brace or
/// whitespace inside.
pub(super) fn has_brace_expansion(text: &str) -> bool {
    let text = chars(text);
    for (open, ch) in text.iter().enumerate() {
        if *ch != '{' {
            continue;
        }
        let run = text[open + 1..]
            .iter()
            .take_while(|ch| **ch != '{' && **ch != '}' && !is_space(**ch))
            .count();
        let close = open + 1 + run;
        if text.get(close) != Some(&'}') {
            continue;
        }
        let body = &text[open + 1..close];
        if body.contains(&',') || body.windows(2).any(|pair| pair == ['.', '.']) {
            return true;
        }
    }
    false
}

/// `[A-Za-z_][A-Za-z0-9_]*` in full.
pub(super) fn is_assignment_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(is_ascii_word)
}

/// `^[A-Za-z_][A-Za-z0-9_]*=`: the word starts with an assignment.
pub(super) fn starts_with_assignment(value: &str) -> bool {
    value
        .split_once('=')
        .is_some_and(|(name, _)| is_assignment_name(name))
}

/// `^GIT_[A-Z_]+=`.
pub(super) fn starts_with_git_assignment(value: &str) -> bool {
    value.strip_prefix("GIT_").is_some_and(|rest| {
        let run = rest
            .chars()
            .take_while(|ch| ch.is_ascii_uppercase() || *ch == '_')
            .count();
        run > 0 && rest[run..].starts_with('=')
    })
}

/// `(^|\s)GIT_[A-Z_]+=` anywhere in `text`.
pub(super) fn has_git_assignment(text: &str) -> bool {
    let text = chars(text);
    (0..text.len()).any(|start| {
        (start == 0 || is_space(text[start - 1]))
            && starts_with_git_assignment(&slice(&text, start, text.len()))
    })
}

/// `GIT_[A-Z_]+=` anywhere in `text`.
pub(super) fn contains_git_assignment(text: &str) -> bool {
    let text = chars(text);
    (0..text.len()).any(|start| starts_with_git_assignment(&slice(&text, start, text.len())))
}

/// `str.strip(chars)`.
pub(super) fn strip_chars<'a>(text: &'a str, set: &str) -> &'a str {
    text.trim_matches(|ch| set.contains(ch))
}

/// `os.path.join(base, path)` for a POSIX path.
pub(super) fn join_path(base: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else if base.is_empty() || base.ends_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_signals_follow_the_python_pattern() {
        let cases = [
            ("git push --force", true),
            ("git push --forc", true),
            ("git push --forced", false),
            ("git push --mir", true),
            ("git push --mirro", false),
            ("git push -vf", true),
            ("git push -f1", false),
            ("git push +main", true),
            ("git push + main", false),
            ("git pushx -f", false),
            ("echo push", false),
        ];
        for (text, expected) in cases {
            assert_eq!(force_push_pattern_in_text(text), expected, "{text}");
        }
    }

    #[test]
    fn shell_interpreters_are_found_as_standalone_names() {
        assert!(has_shell_interpreter("echo x | /bin/sh"));
        assert!(has_shell_interpreter("BASH.EXE -c x"));
        assert!(!has_shell_interpreter("cat payload.sh"));
        assert!(!has_shell_interpreter("ssh host"));
    }
}
