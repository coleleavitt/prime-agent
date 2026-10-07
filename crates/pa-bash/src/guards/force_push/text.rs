//! Character classes and the small text patterns the scan matches.
//!
//! The scan indexes text by character (as the shell reads it), so every helper
//! here works on `&[char]` or `&str` by character. The classes follow Python's
//! `str` semantics the guard was specified in: `\s` is Unicode whitespace
//! (including the information separators U+001C..U+001F) and `\w` is a Unicode
//! letter or digit, or `_`.

use std::sync::LazyLock;

pub(super) use crate::syntax::chars::{is_ascii_word, is_space};
use crate::syntax::pyre::PyRegex;

pub(super) fn chars(text: &str) -> Vec<char> {
    text.chars().collect()
}

pub(super) use crate::syntax::chars::py_slice as slice;

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

static PUSH_IN_TEXT: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"(?<![A-Za-z0-9_])push(?![A-Za-z0-9_])").requiring(&["push"]));
static FORCE_IN_TEXT: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(concat!(
        r"(?<![A-Za-z0-9_])",
        r"(?:--forc(?:e)?(?![A-Za-z0-9_-])|--m(?:ir(?:r(?:or)?)?)?(?![A-Za-z0-9_-])",
        r"|-[A-Za-z]*f(?![A-Za-z0-9_-])|\+[^\s;&|()])",
    ))
});
static SHELL_INTERPRETER_IN_TEXT: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(r"(?i)(?<![A-Za-z0-9_.-])(?:sh|bash|zsh|dash|ksh|fish|tcsh|csh)(?:\.exe)?(?![A-Za-z0-9_.-])")
        .requiring_any_case(&["sh"])
});
static BRACE_EXPANSION: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"\{[^{}\s]*(?:,|\.\.)[^{}\s]*\}"));
static GIT_ASSIGNMENT_WORD: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"(^|\s)GIT_[A-Z_]+="));
static GIT_ASSIGNMENT: LazyLock<PyRegex> = LazyLock::new(|| PyRegex::new(r"GIT_[A-Z_]+="));

/// `_fp_force_push_pattern_in_text`: `push` together with a force signal.
pub(super) fn force_push_pattern_in_text(text: &str) -> bool {
    PUSH_IN_TEXT.is_found(text) && FORCE_IN_TEXT.is_found(text)
}

/// The shells that run a script: the name list of `_FP_SHELL_C_INTERPRETERS`.
pub(super) const SHELL_NAMES: [&str; 8] =
    ["sh", "bash", "zsh", "dash", "ksh", "fish", "tcsh", "csh"];

/// `_FP_SHELL_INTERPRETER_IN_TEXT`: a shell name (any case, optional
/// `.exe`) not glued to a `[A-Za-z0-9_.-]` character on either side, so
/// `/bin/sh` and `./sh` count while `payload.sh` does not.
pub(super) fn has_shell_interpreter(text: &str) -> bool {
    SHELL_INTERPRETER_IN_TEXT.is_found(text)
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
    BRACE_EXPANSION.is_found(text)
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
    GIT_ASSIGNMENT_WORD.is_found(text)
}

/// `GIT_[A-Z_]+=` anywhere in `text`.
pub(super) fn contains_git_assignment(text: &str) -> bool {
    GIT_ASSIGNMENT.is_found(text)
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
