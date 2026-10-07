//! Whole-word mentions (`\b(?:...)\b`) the guards use as cheap gates before
//! a deeper scan: a text that never names `eval` cannot hide an eval payload.

use std::sync::LazyLock;

use super::pyre::{AsText, PyRegex};

/// A set of words mentioned as whole words (Python `\b` boundaries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mention {
    /// `cd`, `pushd`, `popd`.
    Relocator,
    /// `cd`, `pushd`, `popd`, `source`.
    RelocatorOrSource,
    Eval,
    Alias,
    Env,
    /// `env` in any case.
    EnvAnyCase,
    /// `git` in any case.
    GitAnyCase,
    /// `sh`, `bash`, `zsh`, `dash`, `ksh`.
    PosixShell,
    /// Any shell that runs a script, in any case: the POSIX shells plus
    /// `fish`, `tcsh`, `csh`.
    AnyShellAnyCase,
}

static RELOCATOR: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"\b(?:cd|pushd|popd)\b").requiring(&["cd", "pushd", "popd"]));
static RELOCATOR_OR_SOURCE: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(r"\b(?:cd|pushd|popd|source)\b").requiring(&["cd", "pushd", "popd", "source"])
});
static EVAL: LazyLock<PyRegex> = LazyLock::new(|| PyRegex::new(r"\beval\b").requiring(&["eval"]));
static ALIAS: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"\balias\b").requiring(&["alias"]));
static ENV: LazyLock<PyRegex> = LazyLock::new(|| PyRegex::new(r"\benv\b").requiring(&["env"]));
static ENV_ANY_CASE: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"(?i)\benv\b").requiring_any_case(&["env"]));
static GIT_ANY_CASE: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"(?i)\bgit\b").requiring_any_case(&["git"]));
static POSIX_SHELL: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"\b(?:sh|bash|zsh|dash|ksh)\b").requiring(&["sh"]));
static ANY_SHELL_ANY_CASE: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(r"(?i)\b(?:sh|bash|zsh|dash|ksh|fish|tcsh|csh)\b").requiring_any_case(&["sh"])
});

impl Mention {
    /// Whether `text` mentions one of the words.
    pub(crate) fn in_text(self, text: &(impl AsText + ?Sized)) -> bool {
        let pattern = match self {
            Mention::Relocator => &RELOCATOR,
            Mention::RelocatorOrSource => &RELOCATOR_OR_SOURCE,
            Mention::Eval => &EVAL,
            Mention::Alias => &ALIAS,
            Mention::Env => &ENV,
            Mention::EnvAnyCase => &ENV_ANY_CASE,
            Mention::GitAnyCase => &GIT_ANY_CASE,
            Mention::PosixShell => &POSIX_SHELL,
            Mention::AnyShellAnyCase => &ANY_SHELL_ANY_CASE,
        };
        pattern.is_found(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_need_whole_words() {
        assert!(Mention::Relocator.in_text("a && cd x"));
        assert!(!Mention::Relocator.in_text("abcd x"));
        assert!(Mention::GitAnyCase.in_text("GIT push"));
        assert!(!Mention::Env.in_text("ENV x"));
        assert!(Mention::AnyShellAnyCase.in_text("x | /bin/FISH"));
    }
}
