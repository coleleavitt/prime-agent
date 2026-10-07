//! Reads of a known secret directory under the home directory (`~/.ssh`,
//! `~/.gnupg`, `~/.aws`), in the `~` and the `$HOME`/`${HOME}` spellings.

use super::dump::{env_split_words, executed_command_words};
use super::words::{shell_words, Reach};

/// Commands whose operands are read for a secret path (the only readers
/// modeled).
pub(super) const SECRET_READ_COMMANDS: [&str; 2] = ["cat", "echo"];

/// The secret directory names; each must end the name (`.sshfoo`, `.awsrc`
/// are other names).
const SECRET_NAMES: [&str; 3] = [".ssh", ".gnupg", ".aws"];

fn starts_with_at(text: &[char], at: usize, prefix: &str) -> Option<usize> {
    let mut position = at;
    for expected in prefix.chars() {
        if text.get(position) != Some(&expected) {
            return None;
        }
        position += 1;
    }
    Some(position)
}

/// `(?:\.ssh|\.gnupg|\.aws)(?![\w.-])` at `at`.
fn secret_name_at(text: &[char], at: usize) -> bool {
    SECRET_NAMES.iter().any(|name| {
        starts_with_at(text, at, name).is_some_and(|after| {
            !text
                .get(after)
                .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '.' | '-'))
        })
    })
}

/// The index after `$HOME` / `${HOME}` (optional closing brace) at `at`.
fn home_var_at(text: &[char], at: usize) -> Option<usize> {
    if text.get(at) != Some(&'$') {
        return None;
    }
    let mut position = at + 1;
    if text.get(position) == Some(&'{') {
        position += 1;
    }
    position = starts_with_at(text, position, "HOME")?;
    if text.get(position) == Some(&'}') {
        position += 1;
    }
    Some(position)
}

/// The index after one or more `/` at `at` (`/+`), or after exactly one.
fn slashes_at(text: &[char], at: usize, many: bool) -> Option<usize> {
    if text.get(at) != Some(&'/') {
        return None;
    }
    let mut position = at + 1;
    while many && text.get(position) == Some(&'/') {
        position += 1;
    }
    Some(position)
}

/// `~/<secret>` anywhere in masked text (it expands only unquoted).
pub(super) fn tilde_secret_path(text: &[char]) -> bool {
    (0..text.len()).any(|at| {
        text[at] == '~'
            && slashes_at(text, at + 1, false).is_some_and(|after| secret_name_at(text, after))
    })
}

/// `$HOME/<secret>` anywhere in masked text, one optional `"` allowed
/// between the variable and the path (`cat "$HOME"/.ssh/id_rsa`).
pub(super) fn home_var_secret_path(text: &[char]) -> bool {
    (0..text.len()).any(|at| {
        home_var_at(text, at).is_some_and(|mut position| {
            if text.get(position) == Some(&'"') {
                position += 1;
            }
            slashes_at(text, position, false).is_some_and(|after| secret_name_at(text, after))
        })
    })
}

/// `~/+<secret>` at the start of a built word.
fn tilde_word_secret_path(word: &[char]) -> bool {
    word.first() == Some(&'~')
        && slashes_at(word, 1, true).is_some_and(|after| secret_name_at(word, after))
}

/// `$HOME/+<secret>` anywhere in a built word.
fn home_var_word_secret_path(word: &[char]) -> bool {
    (0..word.len()).any(|at| {
        home_var_at(word, at)
            .and_then(|position| slashes_at(word, position, true))
            .is_some_and(|after| secret_name_at(word, after))
    })
}

/// A `$HOME` / `${HOME}` the mask leaves live.
fn live_home_var(text: &[char]) -> bool {
    (0..text.len()).any(|at| home_var_at(text, at).is_some())
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
                && tilde_word_secret_path(&built)
            {
                return true;
            }
            let span = &expanded[word.start.min(expanded.len())..word.end.min(expanded.len())];
            live_home_var(span) && home_var_word_secret_path(&built)
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
        split.iter().any(|word| {
            let chars: Vec<char> = word.chars().collect();
            home_var_word_secret_path(&chars)
        })
    })
}
