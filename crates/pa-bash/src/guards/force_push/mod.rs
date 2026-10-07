//! The force-push guard: a `git push` carrying a force flag (`--force`, `-f`,
//! a `+`-refspec, `--mirror`) is refused while its target is protected: a
//! refspec naming main/master or `@{u}`, every branch under `--all`/`--mirror`,
//! or, when the refspec is implicit, the current upstream probed with `git
//! rev-parse @{u}` (including a branch with no upstream at all).
//!
//! The scan reads the text the way the shell does (continuations joined,
//! quotes and escapes folded, ANSI-C decoded, inline `-c alias.X=` aliases
//! expanded) and refuses what it cannot resolve rather than guessing: a push
//! argument carrying a variable, glob or substitution; a wrapper or payload
//! that hides what runs (`eval`, `sh -c`, `env -S`, ssh, sudo, xargs, a pipe
//! into a shell); configuration that arms a push (`remote.<name>.mirror`,
//! `remote.<name>.push`); a git subcommand outside git's own table; or a
//! command that changes directory first in a way that cannot be replayed. A
//! literal `--force-with-lease`/`--force-if-includes` is never refused. It
//! fails open only where git fails on its own (not a repository).
//!
//! Every nested re-scan spends a deterministic work budget, so hostile nesting
//! is refused instead of wedging the kernel.

mod alias;
mod budget;
mod command_words;
mod config;
mod cwd;
mod lexing;
mod messages;
mod payloads;
mod push;
mod text;
mod upstream;
mod violation;
mod words;

#[cfg(test)]
mod tests;

use std::time::Duration;

use crate::context::GuardContext;
use crate::script::Script;

use alias::unresolvable_git_subcommand;
use budget::{Budget, Scan, ScanStop};
use command_words::{
    ambiguous_env_option, family_violation, unresolvable_command_word_hides_force_push,
};
use config::{mirror_or_push_refspec_configured, unreadable_inline_config};
use lexing::prepare;
use payloads::{nested_payloads_hide_force_push, HiddenIn};
use push::{find_git_push_runs, is_guarded_push, parse_push_args};
use text::{contains_git_assignment, force_push_pattern_in_text, has_word};
use upstream::{ProbeCache, PROBE_TIMEOUT};
use violation::{push_violation, PushSite};
use words::{flattened_text, scan_word_chars};

/// The stderr warning printed (once) when the bypass variable appeared after
/// kernel start.
pub(crate) const LATE_BYPASS_WARNING: Option<&str> = Some(messages::LATE_BYPASS_WARNING);

/// Whether this guard's port is complete.
#[cfg(test)]
pub(crate) const PORTED: bool = true;

/// Refuse a force-push whose target is protected; `Err` is the refusal.
pub(crate) fn check(script: &Script<'_>, context: &GuardContext) -> Result<(), String> {
    check_with(script, context, PROBE_TIMEOUT)
}

/// [`check`] with the upstream probe's timeout given.
fn check_with(
    script: &Script<'_>,
    context: &GuardContext,
    probe_timeout: Duration,
) -> Result<(), String> {
    let budget = Budget::new();
    match scan(script, context, probe_timeout, &budget) {
        Ok(Some(refusal)) => Err(refusal),
        Ok(None) => Ok(()),
        Err(ScanStop::NestingTooDeep) => Err(messages::nesting_refusal()),
        Err(ScanStop::LimitExceeded) => Err(messages::scan_refusal()),
    }
}

/// The scan behind [`check`], under its work budget.
fn scan(
    script: &Script<'_>,
    context: &GuardContext,
    probe_timeout: Duration,
    budget: &Budget,
) -> Scan<Option<String>> {
    let command_text = script.script;
    let prefix = script.prefix.unwrap_or_default();
    let (resolved, normalized_chars, index_map) = prepare(command_text, budget)?;
    let normalized: String = normalized_chars.iter().collect();
    // The gates read the text with quotes intact: a quoted `"eval"` runs.
    match nested_payloads_hide_force_push(&normalized, &resolved, 0, budget)? {
        Some(HiddenIn::Eval) => return Ok(Some(messages::eval_refusal())),
        Some(HiddenIn::ShellC) => return Ok(Some(messages::shell_c_refusal())),
        Some(HiddenIn::EnvSplitString) => return Ok(Some(messages::env_refusal())),
        None => {}
    }
    let trailing_backslashes = command_text.len() - command_text.trim_end_matches('\\').len();
    if trailing_backslashes % 2 == 1 && has_word(&normalized, &["git"], true) {
        // An odd trailing backslash escapes the newline the kernel appends.
        return Ok(Some(messages::refusal(
            "it ends with a line continuation, so the shell joins it with the text that follows in the script \
             the kernel runs",
        )));
    }
    let words = scan_word_chars(&normalized_chars, budget)?;
    if let Some(option) = ambiguous_env_option(&words) {
        if force_push_pattern_in_text(&flattened_text(&words)) {
            return Ok(Some(messages::env_option_refusal(&option)));
        }
    }
    if let Some(reason) = family_violation(&words, command_text, &normalized, budget)? {
        return Ok(Some(messages::refusal(reason)));
    }
    if unresolvable_command_word_hides_force_push(&words, &normalized) {
        return Ok(Some(messages::refusal(
            "its command word is a shell or brace expansion, and the same command line carries a force-push \
             pattern the guard cannot attribute to a command it can see",
        )));
    }
    if let Some(subcommand) = unresolvable_git_subcommand(&words, budget)? {
        return Ok(Some(messages::git_subcommand_refusal(&subcommand)));
    }
    let runs = find_git_push_runs(&words, budget)?;
    if !runs.is_empty() {
        if let Some(refusal) = runs
            .iter()
            .find_map(|run| unreadable_inline_config(run, &words))
        {
            return Ok(Some(refusal));
        }
        if mirror_or_push_refspec_configured(&words) {
            return Ok(Some(messages::mirror_config_refusal()));
        }
    }
    let guarded: Vec<_> = runs
        .iter()
        .map(|run| (run, parse_push_args(&run.tokens, run.push_index)))
        .filter(|(run, args)| run.unresolvable_alias || is_guarded_push(args))
        .collect();
    if guarded.is_empty() {
        return Ok(None);
    }
    let prefix_end = if prefix.is_empty() {
        0
    } else {
        prefix.chars().count() + 1
    };
    let user_command_start = if prefix.is_empty() {
        0
    } else {
        index_map
            .iter()
            .position(|original| *original >= prefix_end)
            .unwrap_or(normalized_chars.len())
    };
    // A GIT_DIR/GIT_WORK_TREE/... assignment in the prefix relocates the
    // repository every later command runs in, like a cd.
    let relocating_prefix = !prefix.is_empty()
        && (has_word(prefix, &["cd", "pushd", "popd"], false) || contains_git_assignment(prefix));
    let kernel_cwd = context.cwd().display().to_string();
    let site = PushSite {
        words: &words,
        normalized: &normalized_chars,
        user_command_start,
        kernel_cwd: &kernel_cwd,
        relocating_prefix,
        context,
        probe_timeout,
    };
    let mut cache = ProbeCache::new();
    for (run, args) in guarded {
        if let Some(refusal) = push_violation(run, &args, &site, &mut cache, budget)? {
            return Ok(Some(refusal));
        }
    }
    Ok(None)
}
