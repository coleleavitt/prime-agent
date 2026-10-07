//! Payloads the shell re-reads as commands: `eval` arguments, quoted
//! `sh -c` (and fish `--command`/`--init-command`) scripts, and `env -S`
//! split strings. The plain word scan sees each as one quoted word, so each
//! is unquoted one layer and scanned again, recursively, under the budget.

use super::alias::unresolvable_git_subcommand;
use super::budget::{Budget, Scan};
use super::command_words::{
    env_long_option, family_violation, is_env_word, unresolvable_command_word_hides_force_push,
    EnvLongOption,
};
use super::config::{mirror_or_push_refspec_configured, unreadable_inline_config};
use super::lexing::{prepare, unquote_one_level};
use super::push::{find_git_push_runs, run_is_guarded};
use super::text::{chars, command_name, has_expansion, has_word, slice, SHELL_NAMES};
use super::words::{literal_words, scan_words, ShellWord};

/// Payload nesting deeper than this is refused rather than missed.
const MAX_PAYLOAD_DEPTH: usize = 10;

/// A `$'...'`/`$"..."` payload word: the guard does not reproduce its split.
fn is_ansi_c(source: &str) -> bool {
    source.starts_with("$'") || source.starts_with("$\"")
}

/// True when a payload the shell re-reads as a command hides a force push:
/// it goes through the same normalization, scan, rules, and nested-payload
/// scans as a top-level command.
pub(super) fn payload_hides_force_push(payload: &str, depth: usize, budget: &Budget) -> Scan<bool> {
    if depth > MAX_PAYLOAD_DEPTH {
        return Ok(true);
    }
    budget.charge()?;
    let (_, normalized, _) = prepare(payload, budget)?;
    let normalized: String = normalized.into_iter().collect();
    let words = scan_words(&normalized, budget)?;
    if family_violation(&words, payload, &normalized, budget)?.is_some() {
        return Ok(true);
    }
    let runs = find_git_push_runs(&words, budget)?;
    if !runs.is_empty()
        && (runs
            .iter()
            .any(|run| unreadable_inline_config(run, &words).is_some())
            || mirror_or_push_refspec_configured(&words))
    {
        return Ok(true);
    }
    if unresolvable_command_word_hides_force_push(&words, &normalized)
        || unresolvable_git_subcommand(&words, budget)?.is_some()
        || runs.iter().any(run_is_guarded)
    {
        return Ok(true);
    }
    nested_payloads_hide_force_push(&normalized, &normalized, depth + 1, budget)
        .map(|found| found.is_some())
}

/// Which kind of nested payload hides a force push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HiddenIn {
    Eval,
    ShellC,
    EnvSplitString,
}

/// The eval, `sh -c`, and `env -S` payload scans of `command`, each behind
/// its cheap textual gate on `gate` (the text with quotes intact: a quoted
/// `"eval"` still executes), in that order.
pub(super) fn nested_payloads_hide_force_push(
    gate: &str,
    command: &str,
    depth: usize,
    budget: &Budget,
) -> Scan<Option<HiddenIn>> {
    if has_word(gate, &["eval"], false) && eval_payloads_hide_force_push(command, depth, budget)? {
        return Ok(Some(HiddenIn::Eval));
    }
    if has_word(gate, &SHELL_NAMES, true)
        && shell_c_payloads_hide_force_push(command, depth, budget)?
    {
        return Ok(Some(HiddenIn::ShellC));
    }
    if has_word(gate, &["env"], true) && env_payloads_hide_force_push(command, depth, budget)? {
        return Ok(Some(HiddenIn::EnvSplitString));
    }
    Ok(None)
}

/// The followers of `words[index]` in its command run, skipping
/// substitution interiors that start their own commands.
fn run_followers(words: &[ShellWord], index: usize) -> impl Iterator<Item = &ShellWord> {
    words[index + 1..]
        .iter()
        .take_while(|word| !word.starts_command || word.contained)
        .filter(|word| !word.starts_command)
}

/// `eval` re-parses its payload: unquote it one layer and rescan, looking at
/// both the raw sources and the folded word values (an alternating eval/sh
/// chain is only reachable through the second look).
pub(super) fn eval_payloads_hide_force_push(
    command: &str,
    depth: usize,
    budget: &Budget,
) -> Scan<bool> {
    if depth > MAX_PAYLOAD_DEPTH {
        return Ok(true);
    }
    let text = chars(command);
    let words = scan_words(command, budget)?;
    for (index, word) in words.iter().enumerate() {
        if word.value != "eval" {
            continue;
        }
        let mut sources = Vec::new();
        let mut values = Vec::new();
        for follower in run_followers(&words, index) {
            let source = slice(&text, follower.start, follower.end);
            if is_ansi_c(&source) {
                return Ok(true);
            }
            sources.push(source);
            values.push(follower.value.as_str());
        }
        let payload = unquote_one_level(&sources.join(" "));
        if has_expansion(&payload)
            || payload_hides_force_push(&payload, depth + 1, budget)?
            || payload_hides_force_push(&values.join(" "), depth + 1, budget)?
            || eval_payloads_hide_force_push(&payload, depth + 1, budget)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A quoted `sh -c`-style payload executes like an eval payload. Short flags
/// may be bundled (any cluster with `c` hands the shell its payload, glued on
/// or as the next word); fish also takes `-C`, `--command` and
/// `--init-command`, and runs every payload it is given.
pub(super) fn shell_c_payloads_hide_force_push(
    command: &str,
    depth: usize,
    budget: &Budget,
) -> Scan<bool> {
    if depth > MAX_PAYLOAD_DEPTH {
        return Ok(true);
    }
    let text = chars(command);
    let words = scan_words(command, budget)?;
    for (index, word) in words.iter().enumerate() {
        let shell_name = command_name(&word.value);
        if !SHELL_NAMES.contains(&shell_name.as_str()) {
            continue;
        }
        let fish = shell_name == "fish";
        let payload_letters = if fish { "cC" } else { "c" };
        let mut c_pending = false;
        let mut attach_offset = 0;
        for follower in &words[index + 1..] {
            if follower.contained {
                continue;
            }
            if follower.starts_command {
                break;
            }
            let token = follower.value.as_str();
            if !c_pending {
                attach_offset = 0;
                if token == "--" {
                    break;
                }
                if let Some(flag) = ["--command=", "--init-command="]
                    .into_iter()
                    .find(|flag| token.starts_with(flag))
                {
                    c_pending = true;
                    attach_offset = flag.len();
                } else if token == "--command" || token == "--init-command" {
                    c_pending = true;
                } else if token.starts_with('-') && token != "-" && !token.starts_with("--") {
                    let letters: Vec<char> = token.chars().collect();
                    if let Some(marker) = letters[1..]
                        .iter()
                        .position(|ch| payload_letters.contains(*ch))
                    {
                        c_pending = true;
                        attach_offset = if letters.len() > 2 + marker {
                            2 + marker
                        } else {
                            0
                        };
                    }
                }
                if c_pending && attach_offset == 0 {
                    continue;
                }
            }
            if !c_pending {
                continue;
            }
            let source = slice(&text, follower.start + attach_offset, follower.end);
            if is_ansi_c(&source) || has_expansion(&source) {
                return Ok(true);
            }
            if (source.starts_with('\'') || source.starts_with('"'))
                && payload_hides_force_push(&unquote_one_level(&source), depth + 1, budget)?
            {
                return Ok(true);
            }
            let value: String = follower.value.chars().skip(attach_offset).collect();
            if payload_hides_force_push(&value, depth + 1, budget)? {
                return Ok(true);
            }
            if !fish {
                break;
            }
            c_pending = false;
        }
    }
    Ok(false)
}

/// Whether one `env -S` payload hides a force push: the raw source (one
/// quoting layer deep) and the folded value are both scanned.
fn env_payload_hides_force_push(
    source: &str,
    value: &str,
    depth: usize,
    budget: &Budget,
) -> Scan<bool> {
    if is_ansi_c(source) || has_expansion(source) || has_expansion(value) {
        return Ok(true);
    }
    if payload_hides_force_push(&unquote_one_level(source), depth + 1, budget)? {
        return Ok(true);
    }
    if value == source {
        return Ok(false);
    }
    payload_hides_force_push(value, depth + 1, budget)
}

/// True when a `-S` payload contributes no command word, so env keeps
/// parsing the options after it (GNU env: `env -S '' -S 'echo hi'` runs
/// `echo hi`).
fn leaves_env_options_open(payload: &str) -> bool {
    let (words, well_formed) = literal_words(payload);
    well_formed
        && words
            .iter()
            .all(|word| word.as_deref().is_some_and(|word| word.starts_with('-')))
}

/// True when such a payload ends in a bare `-S`/`--split-string`, so the
/// next word is that option's payload.
fn reopens_env_payload(payload: &str) -> bool {
    let (words, well_formed) = literal_words(payload);
    if !well_formed || words.iter().any(Option::is_none) {
        return false;
    }
    let Some(Some(last)) = words.last() else {
        return false;
    };
    if last.starts_with("--") {
        return matches!(
            env_long_option(last),
            EnvLongOption::Value {
                option: "split-string",
                glued: None
            }
        );
    }
    last.starts_with('-') && last.chars().skip(1).any(|ch| ch == 'S')
}

/// What one env payload decided.
enum EnvPayload {
    Hides,
    /// The payload left the option walk open; whether the next word is
    /// another payload.
    Open {
        pending: bool,
    },
    /// The payload carried a command word: this env invocation is done.
    Closed,
}

fn judge_env_payload(source: &str, value: &str, depth: usize, budget: &Budget) -> Scan<EnvPayload> {
    if env_payload_hides_force_push(source, value, depth, budget)? {
        return Ok(EnvPayload::Hides);
    }
    if leaves_env_options_open(value) {
        return Ok(EnvPayload::Open {
            pending: reopens_env_payload(value),
        });
    }
    Ok(EnvPayload::Closed)
}

/// True when an `env -S`/`--split-string` payload hides a force push (env
/// splits the one word into the argv git receives).
pub(super) fn env_payloads_hide_force_push(
    command: &str,
    depth: usize,
    budget: &Budget,
) -> Scan<bool> {
    if depth > MAX_PAYLOAD_DEPTH {
        return Ok(true);
    }
    let text = chars(command);
    let words = scan_words(command, budget)?;
    for (index, word) in words.iter().enumerate() {
        if !is_env_word(&word.value) {
            continue;
        }
        let mut payload_pending = false;
        for follower in &words[index + 1..] {
            if follower.contained {
                continue;
            }
            if follower.starts_command {
                break;
            }
            let token = follower.value.as_str();
            let source = slice(&text, follower.start, follower.end);
            let verdict = if payload_pending {
                judge_env_payload(&source, token, depth, budget)?
            } else if token == "--" {
                break;
            } else if token.starts_with("--") {
                match env_long_option(token) {
                    EnvLongOption::Value {
                        option: "split-string",
                        glued: None,
                    } => {
                        payload_pending = true;
                        continue;
                    }
                    EnvLongOption::Value {
                        option: "split-string",
                        glued: Some(glued),
                    } => {
                        let operand_at = source.find('=').map_or(0, |at| at + 1);
                        judge_env_payload(&source[operand_at..], &glued, depth, budget)?
                    }
                    EnvLongOption::Value { .. }
                    | EnvLongOption::Ambiguous
                    | EnvLongOption::Other => continue,
                }
            } else if let Some(short) = token.strip_prefix('-') {
                let Some(at) = short.chars().position(|ch| ch == 'S') else {
                    continue;
                };
                let attached: String = short.chars().skip(at + 1).collect();
                if attached.is_empty() {
                    payload_pending = true;
                    continue;
                }
                let offset = follower.start + 1 + at + 1;
                judge_env_payload(
                    &slice(&text, offset, follower.end),
                    &attached,
                    depth,
                    budget,
                )?
            } else {
                continue;
            };
            match verdict {
                EnvPayload::Hides => return Ok(true),
                EnvPayload::Open { pending } => payload_pending = pending,
                EnvPayload::Closed => break,
            }
        }
    }
    Ok(false)
}
