//! The command words and options the guard knows by name.

use super::pyos::basename;

/// Shells whose `-c` payload, script argument or stdin is shell code.
pub(super) const SHELL_C_INTERPRETERS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];

/// Command words that hand their arguments to a program: an unresolvable
/// word in one of their runs could still be chmod/chown.
pub(super) const UNRESOLVABLE_COMMAND_EXECUTORS: [&str; 16] = [
    "xargs", "sudo", "env", "nohup", "exec", "command", "find", "nice", "timeout", "setsid",
    "stdbuf", "ionice", "parallel", "time", "strace", "valgrind",
];

/// Words that hold a run's command slot without being the command itself:
/// grouping tokens, group keywords, dispatchers and compound introducers.
pub(super) const COMMAND_SLOT_NOISE: [&str; 16] = [
    "{", "}", "(", ")", "then", "do", "else", "elif", "!", "command", "builtin", "if", "while",
    "until", "time", "coproc",
];

/// Heads whose `BASH_ENV=` operand arms the file before the command runs.
pub(super) const ENV_ARMING_HEADS: [&str; 6] =
    ["env", "export", "declare", "typeset", "sudo", "nohup"];

/// Wrappers that execute a process substitution's output as shell code.
pub(super) const PROC_SUB_WRAPPERS: [&str; 7] = ["sh", "bash", "zsh", "dash", "ksh", "source", "."];

/// Heads that introduce such a wrapper from a later position.
pub(super) const PROC_SUB_INTRODUCERS: [&str; 4] = ["env", "nohup", "exec", "sudo"];

/// Wrappers that execute a named script file (argument or stdin redirect).
pub(super) const SCRIPT_INPUT_WRAPPERS: [&str; 7] =
    ["sh", "bash", "zsh", "dash", "ksh", "source", "."];

/// Long options of the shell wrappers that take no value.
pub(super) const WRAPPER_LONG_OPTIONS: [&str; 10] = [
    "--posix",
    "--restricted",
    "--noprofile",
    "--norc",
    "--verbose",
    "--debug",
    "--login",
    "--interactive",
    "--help",
    "--version",
];

/// Long options of the shell wrappers that take the next word as a value.
pub(super) const WRAPPER_LONG_OPTIONS_WITH_VALUE: [&str; 2] = ["--rcfile", "--init-file"];

/// Every unambiguous GNU abbreviation of `--recursive`.
const RECURSIVE_LONG_FLAGS: [&str; 7] = [
    "--rec",
    "--recur",
    "--recurse",
    "--recurs",
    "--recursi",
    "--recursiv",
    "--recursive",
];

/// Whether the word invokes chmod/chown (slash-qualified forms included).
pub(super) fn is_chmod_chown_word(value: &str) -> bool {
    matches!(basename(value), "chmod" | "chown")
}

/// Whether the word's basename is one of `names`.
pub(super) fn named(value: &str, names: &[&str]) -> bool {
    names.contains(&basename(value))
}

/// Whether a short option cluster (`-x`, not `-`, not `--x`).
pub(super) fn is_short_option(token: &str) -> bool {
    token.starts_with('-') && token != "-" && !token.starts_with("--")
}

/// Whether a token run carries a recursive flag before any `--`: `-R`
/// anywhere in a short cluster, `--recursive` or one of its abbreviations.
pub(super) fn is_recursive_token_run<S: AsRef<str>>(tokens: &[S]) -> bool {
    for token in tokens {
        let token = token.as_ref();
        if token == "--" {
            break;
        }
        if RECURSIVE_LONG_FLAGS.contains(&token) {
            return true;
        }
        if token.starts_with('-') && !token.starts_with("--") && token[1..].contains('R') {
            return true;
        }
    }
    false
}

/// The short option letters a shell option cluster sets: `-o`/`-O` take the
/// rest of their own cluster as the option name.
fn shell_short_option_flags(token: &str) -> &str {
    let flags = &token[1..];
    let cut = ["o", "O"]
        .iter()
        .filter_map(|option| flags.find(option))
        .min()
        .unwrap_or(flags.len());
    &flags[..cut]
}

/// Whether a shell option word makes the shell read a startup file first (a
/// login or interactive shell).
pub(super) fn is_shell_startup_option(token: &str) -> bool {
    if token == "--login" || token == "--interactive" {
        return true;
    }
    if is_short_option(token) {
        let flags = shell_short_option_flags(token);
        return flags.contains('l') || flags.contains('i');
    }
    false
}
