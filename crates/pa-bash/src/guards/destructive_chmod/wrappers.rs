//! Shell wrappers that execute content the guard cannot scan: a process
//! substitution or here-string fed to a wrapper, a bare wrapper reading a
//! pipe, a script file from outside the workspace, startup files of a login
//! or interactive shell, and a command prefix that relocates.

use super::location::{
    assigns_path, effective_cwd, path_hit, resolve_operand, script_input_violation, EffectiveCwd,
};
use super::patterns::{before_separator, is_assignment_word, stdin_redirect_targets};
use super::pyos::{basename, is_file, is_space, realpath};
use super::vocabulary::{
    is_shell_startup_option, is_short_option, named, COMMAND_SLOT_NOISE, PROC_SUB_INTRODUCERS,
    PROC_SUB_WRAPPERS, SCRIPT_INPUT_WRAPPERS, SHELL_C_INTERPRETERS, UNRESOLVABLE_COMMAND_EXECUTORS,
    WRAPPER_LONG_OPTIONS, WRAPPER_LONG_OPTIONS_WITH_VALUE,
};
use super::words::{py_slice, run_followers, ShellWord};
use super::{Chmod, ScriptReason};

fn contains_feed(region: &[char]) -> bool {
    region
        .windows(2)
        .any(|pair| pair == ['<', '('] || pair == ['>', '('])
        || region.windows(3).any(|triple| triple == ['<', '<', '<'])
}

/// Whether a shell wrapper's first argument is a process substitution, or
/// its stdin a here-string: it executes content that cannot be scanned. A
/// `-c` payload governs instead.
pub(super) fn process_substitution_feeds_wrapper(normalized: &[char], words: &[ShellWord]) -> bool {
    if !contains_feed(normalized) {
        return false;
    }
    let mut head: Option<&ShellWord> = None;
    for (index, word) in words.iter().enumerate() {
        if word.heads_run() {
            head = Some(word);
        }
        if !named(&word.value, &PROC_SUB_WRAPPERS) {
            continue;
        }
        let introduced = word.starts_command
            || head.is_some_and(|head| {
                is_assignment_word(&head.value) || named(&head.value, &PROC_SUB_INTRODUCERS)
            });
        if !introduced {
            continue;
        }
        let mut rest_from = word.end;
        let mut governed = false;
        for follower in run_followers(words, index) {
            let token = follower.value.as_str();
            if is_short_option(token) {
                if token[1..].contains('c') {
                    governed = true;
                }
                rest_from = follower.end;
                continue;
            }
            if token == "--" {
                rest_from = follower.end;
                continue;
            }
            break;
        }
        if governed {
            continue;
        }
        if contains_feed(before_separator(py_slice(
            normalized,
            rest_from,
            normalized.len(),
        ))) {
            return true;
        }
    }
    false
}

/// `text.rstrip()` of a character slice.
fn rstrip(text: &[char]) -> &[char] {
    let end = text
        .iter()
        .rposition(|c| !is_space(*c))
        .map_or(0, |index| index + 1);
    &text[..end]
}

/// Whether a bare shell wrapper (no `-c`, no script argument) takes its
/// commands from a pipe, possibly through a group opener.
pub(super) fn shell_wrapper_reads_pipe(normalized: &[char], words: &[ShellWord]) -> bool {
    for (index, word) in words.iter().enumerate() {
        let before = rstrip(py_slice(normalized, 0, word.start));
        let introduced_by_opener = matches!(before.last(), Some('{' | '('));
        if !word.starts_command && !introduced_by_opener {
            continue;
        }
        if !named(&word.value, &SHELL_C_INTERPRETERS) {
            continue;
        }
        let Some(pipe) = before.iter().rposition(|c| *c == '|') else {
            continue;
        };
        let between: Vec<char> = before[pipe + 1..]
            .iter()
            .copied()
            .filter(|c| !is_space(*c))
            .collect();
        if !between.is_empty() && between.iter().any(|c| !matches!(c, '{' | '(')) {
            continue;
        }
        let mut c_payload = false;
        let mut script_arg = false;
        let mut skip_next = false;
        for follower in run_followers(words, index) {
            let token = follower.value.as_str();
            if skip_next {
                skip_next = false;
                continue;
            }
            if is_short_option(token) {
                if token[1..].contains('c') {
                    c_payload = true;
                }
                if token.ends_with(['o', 'O']) {
                    skip_next = true;
                }
                continue;
            }
            if WRAPPER_LONG_OPTIONS_WITH_VALUE.contains(&token) {
                skip_next = true;
                continue;
            }
            if token == "--" || follower.contained {
                continue;
            }
            script_arg = true;
            break;
        }
        if !c_payload && !script_arg {
            return true;
        }
    }
    false
}

/// A script-input refusal reason; a candidate or option text that spells
/// one of the named reasons reads as that reason, as it always has.
fn named_reason(text: &str) -> ScriptReason {
    match text {
        "relocation" => ScriptReason::Relocation,
        "startup" => ScriptReason::Startup,
        "unscanned_script" => ScriptReason::UnscannedScript,
        _ => ScriptReason::Other,
    }
}

impl Chmod<'_> {
    /// Whether the command prefix moves the shell (cd/pushd/popd) in any
    /// spelling the shell builds.
    pub(super) fn prefix_relocates(&self) -> Result<bool, String> {
        let Some(prefix) = self.prefix.filter(|prefix| !prefix.is_empty()) else {
            return Ok(false);
        };
        if super::patterns::has_word(prefix, &["cd", "pushd", "popd"]) {
            return Ok(true);
        }
        let prefix: Vec<char> = prefix.chars().collect();
        let prepared = super::prepare(&prefix)?;
        Ok(prepared.words.iter().any(|word| {
            word.starts_command && matches!(word.value.as_str(), "cd" | "pushd" | "popd")
        }))
    }

    /// Why a bare shell wrapper executes a script the guard cannot scan, or
    /// sources startup files: a script argument or stdin redirection from
    /// outside the workspace (cd relocations replayed, slash-free names
    /// searched on `PATH`), an unknown long option, a login/interactive
    /// shell, or an executor feeding a bare interpreter its script.
    #[expect(
        clippy::too_many_lines,
        reason = "one walk per wrapper word; the gates are ordered and share the follower state"
    )]
    pub(super) fn unscanned_wrapper_script_reason(
        &self,
        raw: &[char],
        normalized: &[char],
        words: &[ShellWord],
        user_command_start: usize,
    ) -> Result<Option<ScriptReason>, String> {
        let locations = &self.locations;
        let prefix_relocates = self.prefix_relocates()?;
        let mut head: Option<&ShellWord> = None;
        for (index, word) in words.iter().enumerate() {
            if word.heads_run() {
                head = Some(word);
            }
            if !named(&word.value, &SCRIPT_INPUT_WRAPPERS) {
                continue;
            }
            let introduced = word.starts_command
                || head.is_some_and(|head| {
                    is_assignment_word(&head.value)
                        || named(&head.value, &UNRESOLVABLE_COMMAND_EXECUTORS)
                        || COMMAND_SLOT_NOISE.contains(&head.value.as_str())
                });
            if !introduced {
                continue;
            }
            let reads_startup_files = named(&word.value, &SHELL_C_INTERPRETERS);
            if prefix_relocates {
                return Ok(Some(ScriptReason::Relocation));
            }
            let mut script_word: Option<&ShellWord> = None;
            let mut governed = false;
            let mut skip_next = false;
            for follower in run_followers(words, index) {
                let token = follower.value.as_str();
                if skip_next {
                    skip_next = false;
                    continue;
                }
                if reads_startup_files && is_shell_startup_option(token) {
                    return Ok(Some(ScriptReason::Startup));
                }
                if is_short_option(token) {
                    if token[1..].contains('c') {
                        governed = true;
                        break;
                    }
                    skip_next = token.ends_with(['o', 'O']);
                    continue;
                }
                if token == "--" {
                    continue;
                }
                if WRAPPER_LONG_OPTIONS_WITH_VALUE.contains(&token) {
                    skip_next = true;
                    continue;
                }
                if token.starts_with("--") {
                    if !WRAPPER_LONG_OPTIONS.contains(&token) {
                        return Ok(Some(named_reason(token)));
                    }
                    continue;
                }
                if follower.contained {
                    continue;
                }
                script_word = Some(follower);
                break;
            }
            if governed {
                continue;
            }
            let mut candidates: Vec<String> =
                script_word.iter().map(|word| word.value.clone()).collect();
            let raw_end = script_word.map_or(word.end, |script| script.end);
            candidates.extend(stdin_redirect_targets(before_separator(py_slice(
                raw,
                raw_end,
                raw.len(),
            ))));
            if candidates.is_empty() && named(&word.value, &SHELL_C_INTERPRETERS) {
                let mut slot_holder = head;
                for before in words[..index].iter().rev() {
                    if before.starts_command {
                        slot_holder = Some(before);
                        break;
                    }
                    if is_assignment_word(&before.value)
                        || COMMAND_SLOT_NOISE.contains(&before.value.as_str())
                    {
                        continue;
                    }
                    if before.value.starts_with('-') && before.value != "-" {
                        continue;
                    }
                    slot_holder = Some(before);
                    break;
                }
                if slot_holder.is_some_and(|slot_holder| {
                    named(&slot_holder.value, &UNRESOLVABLE_COMMAND_EXECUTORS)
                }) {
                    return Ok(Some(ScriptReason::UnscannedScript));
                }
                continue;
            }
            let base = match effective_cwd(
                locations,
                py_slice(normalized, 0, word.start),
                user_command_start,
            ) {
                EffectiveCwd::Unresolvable => return Ok(Some(ScriptReason::Relocation)),
                EffectiveCwd::Workspace => locations.workspace.clone(),
                EffectiveCwd::Dir(dir) => dir,
            };
            let reader = basename(&word.value);
            for candidate in candidates {
                if candidate.starts_with('-') {
                    continue;
                }
                let resolved = if matches!(reader, "source" | ".") && !candidate.contains('/') {
                    if assigns_path(normalized) {
                        return Ok(Some(named_reason(&candidate)));
                    }
                    let Some(found) = path_hit(locations, &candidate) else {
                        continue;
                    };
                    realpath(locations.context, &found)
                } else {
                    let resolved = resolve_operand(locations, &candidate, &base);
                    if SHELL_C_INTERPRETERS.contains(&reader)
                        && !candidate.contains('/')
                        && resolved
                            .as_deref()
                            .is_some_and(|resolved| !is_file(locations.context, resolved))
                    {
                        if assigns_path(normalized) {
                            return Ok(Some(named_reason(&candidate)));
                        }
                        if let Some(found) = path_hit(locations, &candidate) {
                            let hit_real = realpath(locations.context, &found);
                            if script_input_violation(locations, hit_real.as_deref()) {
                                return Ok(Some(named_reason(&candidate)));
                            }
                        }
                    }
                    resolved
                };
                if script_input_violation(locations, resolved.as_deref()) {
                    return Ok(Some(named_reason(&candidate)));
                }
            }
        }
        Ok(None)
    }
}
