//! Command words the guard cannot resolve, and the "unresolvable argv"
//! family they belong to.
//!
//! A shell text guard cannot follow an expansion in command position, an
//! unmodeled wrapper (ssh, sudo, docker, ...), or an execution conduit (a
//! pipe or redirect into a shell, a here-string, xargs driving a shell) to
//! the command that really runs, so a force-push pattern next to one is
//! refused.

use std::collections::BTreeSet;

use super::alias::WORD_EDGE_NOISE;
use super::budget::{Budget, Scan};
use super::lexing::{mask_redirections, unquoted_text};
use super::push::{is_guarded_push, parse_push_args};
use super::text::{
    chars, command_name, force_push_pattern_in_text, has_brace_expansion, has_expansion,
    has_shell_interpreter, slice, starts_with_assignment, strip_chars,
};
use super::words::{flattened_text, invocation_tokens, ShellWord};

/// Wrappers in front of a git word that the invocation walk looks through.
pub(super) const COMMAND_WRAPPERS: [&str; 9] = [
    "sudo", "env", "command", "builtin", "nice", "nohup", "stdbuf", "setsid", "time",
];

/// Wrappers that can change what or where the command runs (a child process,
/// a container, another host, another user, a changed root).
pub(super) const UNMODELED_WRAPPERS: [&str; 22] = [
    "ssh",
    "chroot",
    "timeout",
    "parallel",
    "docker",
    "podman",
    "nsenter",
    "unshare",
    "bwrap",
    "firejail",
    "flatpak",
    "systemd-run",
    "runuser",
    "su",
    "doas",
    "sudo",
    "time",
    "setsid",
    "stdbuf",
    "nohup",
    "nice",
    "xargs",
];

const ENV_COMMAND_NAMES: [&str; 2] = ["env", "env.exe"];

pub(super) fn is_unmodeled_wrapper(value: &str) -> bool {
    UNMODELED_WRAPPERS.contains(&command_name(value).as_str())
}

/// GNU env's long options (coreutils 9.11), resolved by unique prefix like
/// `getopt_long`.
const ENV_LONG_OPTIONS: [&str; 13] = [
    "argv0",
    "unset",
    "chdir",
    "split-string",
    "ignore-environment",
    "null",
    "debug",
    "block-signal",
    "default-signal",
    "ignore-signal",
    "list-signal-handling",
    "help",
    "version",
];
const ENV_LONG_VALUE_OPTIONS: [&str; 4] = ["argv0", "unset", "chdir", "split-string"];

/// How a `--name[=operand]` env word resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EnvLongOption {
    /// A value-taking option, with the operand glued on by `=` (`None` when
    /// the operand is the next word).
    Value {
        option: &'static str,
        glued: Option<String>,
    },
    /// A prefix of more than one option (env rejects it).
    Ambiguous,
    /// A valueless option, no option at all, or the bare `--`.
    Other,
}

pub(super) fn env_long_option(value: &str) -> EnvLongOption {
    let rest = &value[2..];
    let (name, glued) = match rest.split_once('=') {
        Some((name, glued)) => (name, Some(glued.to_string())),
        None => (rest, None),
    };
    if name.is_empty() {
        return EnvLongOption::Other;
    }
    let mut matches = ENV_LONG_OPTIONS
        .iter()
        .filter(|option| option.starts_with(name));
    match (matches.next(), matches.next()) {
        (Some(_), Some(_)) => EnvLongOption::Ambiguous,
        (Some(option), None) if ENV_LONG_VALUE_OPTIONS.contains(option) => {
            EnvLongOption::Value { option, glued }
        }
        (Some(_) | None, _) => EnvLongOption::Other,
    }
}

/// env's value-taking options (the separate-operand spellings).
const ENV_VALUE_OPTIONS: [&str; 10] = [
    "-u",
    "--unset",
    "-C",
    "--chdir",
    "-S",
    "--split-string",
    "-a",
    "--argv0",
    "-P",
    "--env0-from",
];

/// Whether a wrapper option word takes its value from the next word
/// (`env -u NAME`, `env -vu NAME`, `env --uns NAME`), the way getopt reads it.
fn takes_separate_operand(value: &str, wrapper: &str) -> bool {
    if wrapper != "env" {
        return false; // `command` and `builtin` take no value options
    }
    if ENV_VALUE_OPTIONS.contains(&value) {
        return true;
    }
    if value.starts_with("--") {
        return matches!(
            env_long_option(value),
            EnvLongOption::Value { glued: None, .. }
        );
    }
    let letters: Vec<char> = value.chars().collect();
    if letters.len() > 2 && letters[0] == '-' && letters[1] != '-' {
        return letters[1..]
            .iter()
            .position(|ch| "uCSaP".contains(*ch))
            .is_some_and(|offset| letters.len() == 2 + offset);
    }
    false
}

/// Indices of command words the guard cannot resolve: an expansion (quoted
/// or not), an unquoted brace expansion, or an unmodeled wrapper, after
/// stepping over assignment prefixes and the modeled wrappers (`env`,
/// `command`, `builtin`) with their options. A substitution interior is its
/// own command text and is judged too.
fn unresolvable_command_words(words: &[ShellWord], command: &[char]) -> BTreeSet<usize> {
    let mut found = BTreeSet::new();
    let total = words.len();
    for index in 0..total {
        if !words[index].starts_command {
            continue;
        }
        let interior = words[index].contained;
        let run_end = if interior {
            (index..total)
                .find(|probe| !words[*probe].contained)
                .unwrap_or(total)
        } else {
            total
        };
        let in_run = |probe: usize| probe < run_end && (interior || !words[probe].contained);
        let mut probe = index;
        let mut wrapper = String::new();
        while in_run(probe) {
            let value = words[probe].value.as_str();
            if starts_with_assignment(value) || value.starts_with('-') {
                probe += 1;
                if takes_separate_operand(value, &wrapper) && in_run(probe) {
                    probe += 1;
                }
                continue;
            }
            let name = command_name(value);
            if COMMAND_WRAPPERS.contains(&name.as_str()) && !is_unmodeled_wrapper(value) {
                wrapper = name;
                probe += 1;
                continue;
            }
            break;
        }
        if probe >= run_end || (probe != index && words[probe].starts_command) {
            continue;
        }
        let candidate = &words[probe];
        let span = slice(command, candidate.start, candidate.end);
        if has_expansion(&unquoted_text(&span, true))
            || has_brace_expansion(&unquoted_text(&span, false))
            || is_unmodeled_wrapper(strip_chars(&candidate.value, WORD_EDGE_NOISE))
        {
            found.insert(probe);
        }
    }
    found
}

/// True when a shell interpreter can read the command it runs from stdin.
fn execution_conduit(words: &[ShellWord], text: &str) -> bool {
    if text.contains("<<<") {
        return true;
    }
    if !has_shell_interpreter(text) {
        return false;
    }
    if text.contains(['|', '<']) {
        return true;
    }
    words
        .iter()
        .any(|word| command_name(&word.value) == "xargs")
}

/// Why this command belongs to the unresolvable-argv family, or `None`.
///
/// `text` is the command as written (a conduit is made of the redirection
/// characters the masker removes); `scan_text` is the text `words` were
/// scanned from, which their spans index into.
pub(super) fn family_violation(
    words: &[ShellWord],
    text: &str,
    scan_text: &str,
    budget: &Budget,
) -> Scan<Option<&'static str>> {
    let command = chars(&mask_redirections(scan_text, budget)?);
    if execution_conduit(words, text) {
        return Ok(Some(
            "a shell reads the command it runs from a pipe, a here-string, or a redirect, so the \
             guard cannot see what executes",
        ));
    }
    if !force_push_pattern_in_text(&flattened_text(words)) {
        return Ok(None);
    }
    let unresolvable = unresolvable_command_words(words, &command);
    if unresolvable.is_empty() {
        return Ok(None);
    }
    if unresolvable
        .iter()
        .any(|index| is_unmodeled_wrapper(&words[*index].value))
    {
        return Ok(Some(
            "an unmodeled wrapper (ssh, chroot, timeout, sudo, docker, xargs, ...) is in command \
             position next to a force-push pattern, and the guard cannot see what it runs or where",
        ));
    }
    Ok(Some(
        "its command word is argv the guard cannot resolve (an expansion or an unquoted brace \
         expansion) next to a force-push pattern, so what it runs is decided at run time",
    ))
}

/// True when an unresolvable command word (not a wrapper: the family rule
/// covers those) sits in a run that also carries a guarded `push`.
pub(super) fn unresolvable_command_word_hides_force_push(
    words: &[ShellWord],
    command: &str,
) -> bool {
    let command = chars(command);
    unresolvable_command_words(words, &command)
        .into_iter()
        .filter(|index| !is_unmodeled_wrapper(&words[*index].value))
        .any(|index| {
            let tokens = invocation_tokens(words, index);
            (1..tokens.len())
                .filter(|push_index| tokens[*push_index] == "push")
                .any(|push_index| is_guarded_push(&parse_push_args(&tokens, push_index)))
        })
}

/// Whether `value` names env (case-folded basename).
pub(super) fn is_env_word(value: &str) -> bool {
    ENV_COMMAND_NAMES.contains(&command_name(value).as_str())
}

/// The first ambiguous GNU env long-option abbreviation of an env
/// invocation (`--i` is both --ignore-environment and --ignore-signal), or
/// `None`.
pub(super) fn ambiguous_env_option(words: &[ShellWord]) -> Option<String> {
    for (index, word) in words.iter().enumerate() {
        if !is_env_word(&word.value) {
            continue;
        }
        for follower in &words[index + 1..] {
            if follower.contained {
                continue;
            }
            if follower.starts_command || follower.value == "--" {
                break;
            }
            if follower.value.starts_with("--")
                && env_long_option(&follower.value) == EnvLongOption::Ambiguous
            {
                return Some(follower.value.clone());
            }
        }
    }
    None
}
