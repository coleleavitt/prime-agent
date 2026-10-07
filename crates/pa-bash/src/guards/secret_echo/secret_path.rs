//! Reads of a known secret directory under the home directory (`~/.ssh`,
//! `~/.gnupg`, `~/.aws`), in the `~` and the `$HOME`/`${HOME}` spellings.

use super::dump::{env_split_words, executed_command_words};
use super::words::{shell_words, Reach};
use crate::syntax::pyre::PyRegex;
use std::sync::LazyLock;

/// Commands whose operands are read for a secret path (the only readers
/// modeled).
pub(super) const SECRET_READ_COMMANDS: [&str; 2] = ["cat", "echo"];

/// A secret directory name; the lookahead keeps a longer name (`.sshfoo`,
/// `.awsrc`) from matching.
const SECRET_HOME_NAME: &str = r"(?:\.ssh|\.gnupg|\.aws)(?![\w.-])";

/// `~/<secret>` (it expands only unquoted, so this reads masked text).
static TILDE_SECRET_PATH: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(&format!("~/(?:{SECRET_HOME_NAME})")).requiring(&["~"]));
/// `$HOME/<secret>`; a double-quoted `$HOME` may close its quote before the
/// path (`cat "$HOME"/.ssh/id_rsa`).
static HOME_VAR_SECRET_PATH: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(&format!(r#"\$\{{?HOME\}}?"?(?:/(?:{SECRET_HOME_NAME}))"#)).requiring(&["HOME"])
});
/// The same two rules on the word the shell builds, where quotes are gone and
/// a run of slashes is one slash to the kernel.
static TILDE_WORD_SECRET_PATH: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(&format!("~/+(?:{SECRET_HOME_NAME})")));
static HOME_VAR_WORD_SECRET_PATH: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(&format!(r"\$\{{?HOME\}}?/+(?:{SECRET_HOME_NAME})")).requiring(&["HOME"])
});
/// A `$HOME` the mask leaves live.
static LIVE_HOME_VAR: LazyLock<PyRegex> = LazyLock::new(|| PyRegex::new(r"\$\{?HOME\}?"));

/// `~/<secret>` anywhere in masked text.
pub(super) fn tilde_secret_path(text: &[char]) -> bool {
    TILDE_SECRET_PATH.is_found(text)
}

/// `$HOME/<secret>` anywhere in masked text.
pub(super) fn home_var_secret_path(text: &[char]) -> bool {
    HOME_VAR_SECRET_PATH.is_found(text)
}

/// Whether a word the shell builds in `command[start..end]` names a secret
/// path. The built word is the concatenation the quotes hid
/// (`cat ~/".ssh"/id_rsa` reads the key). A `~` expands only as the word's
/// first character with the raw next character the path's `/` (so
/// `cat ~'/'.ssh/id_rsa` is text); a `$HOME` must be live in the mask that
/// leaves double quotes live (`cat '$HOME/.ssh/id_rsa'` is text).
pub(super) fn live_secret_path_word(
    command: &[char],
    literal: &[char],
    expanded: &[char],
    start: usize,
    end: usize,
) -> bool {
    shell_words(command, start, end, Reach::All)
        .into_iter()
        .any(|word| {
            let built: Vec<char> = word.text.chars().collect();
            if literal.get(word.start) == Some(&'~')
                && command.get(word.start + 1) == Some(&'/')
                && TILDE_WORD_SECRET_PATH.match_start(&built).is_some()
            {
                return true;
            }
            let span = &expanded[word.start.min(expanded.len())..word.end.min(expanded.len())];
            LIVE_HOME_VAR.is_found(span) && HOME_VAR_WORD_SECRET_PATH.is_found(&built)
        })
}

/// Whether an `env` invocation runs `cat`/`echo` (`env cat ~/.ssh/id_rsa`).
pub(super) fn executor_reader(words: &[String]) -> bool {
    words[0] == "env"
        && executed_command_words(&words[1..])
            .first()
            .is_some_and(|word| SECRET_READ_COMMANDS.contains(&word.as_str()))
}

/// Whether an `env -S` operand names a `${HOME}` secret path: `env` expands
/// `${VARNAME}` in the operand it splits itself, while a `~` there stays text.
pub(super) fn split_operand_secret_path(words: &[String]) -> bool {
    if words[0] != "env" {
        return false;
    }
    env_split_words(&words[1..]).is_some_and(|split| {
        split
            .iter()
            .any(|word| HOME_VAR_WORD_SECRET_PATH.is_found(word.as_str()))
    })
}
