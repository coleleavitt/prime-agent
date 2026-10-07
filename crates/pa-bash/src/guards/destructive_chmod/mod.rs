//! The recursive chmod/chown workspace-escape guard: a recursive chmod/chown
//! whose operands could reach outside the kernel workspace, the home
//! directory, a dot-directory or dotfile, or the filesystem root is refused,
//! and so is every form the scan cannot resolve statically (command names
//! built from expansion, quoted wrapper payloads, `env -S` strings, traps,
//! aliases, function definitions, process substitutions and here-strings
//! fed to shells, pipe-fed or startup-sourcing shells, scripts from outside
//! the workspace, `BASH_ENV` arming, `hash -p` registrations, relocating
//! executor chains, CDPATH-affected or unreplayable `cd`s, and a `PATH`
//! that can shadow the command word).

mod invocations;
mod location;
mod messages;
mod normalize;
mod patterns;
mod payloads;
mod pyos;
mod vocabulary;
mod words;
mod wrappers;

#[cfg(test)]
mod tests;

use crate::context::GuardContext;
use crate::script::Script;

use invocations::{
    env_option_values, find_invocations, hash_registered_command_names, operand_words,
    region_words, wrapper_chain_groups,
};
use location::{
    effective_cwd, operand_violation, path_can_shadow_command_lookup, resolve_operand,
    EffectiveCwd, Locations,
};
use messages::PayloadReason;
use normalize::{
    locate_heredoc, mask_shell_redirections, normalize_line_continuations, strip_shell_escapes,
};
use patterns::{has_word, redirect_operators};
use payloads::{bash_env_words_arm_shell_code, function_definition_could_recurse, PayloadKind};
use pyos::{basename, strip};
use vocabulary::{is_chmod_chown_word, named, SCRIPT_INPUT_WRAPPERS, SHELL_C_INTERPRETERS};
use words::{py_slice, scan_shell_words, substitution_spans, word_before, ShellWord};
use wrappers::{process_substitution_feeds_wrapper, shell_wrapper_reads_pipe};

/// The stderr warning printed (once) when the bypass variable appeared after kernel start.
pub(crate) const LATE_BYPASS_WARNING: Option<&str> = Some(
    "prime-agent bash: PI_BASH_ALLOW_DESTRUCTIVE_CHMOD appeared after kernel start and is ignored; the recursive chmod/chown guard only honors it when the kernel is started with it set.",
);

/// Whether this guard's port is complete.
#[cfg(test)]
pub(crate) const PORTED: bool = true;

/// Heredoc bodies scanned as shell code recurse one guard pass per wrapper
/// level; deeper hostile nesting refuses.
const MAX_HEREDOC_NESTING: usize = 25;

/// Why a shell wrapper's script input cannot be scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptReason {
    /// The shell moves before the wrapper reads its script.
    Relocation,
    /// A login or interactive shell sources startup files.
    Startup,
    /// An executor feeds a bare interpreter its script at runtime.
    UnscannedScript,
    /// A script path outside the workspace (or unresolvable), or an unknown
    /// wrapper option.
    Other,
}

/// One check: where it measures against and the trusted command prefix.
struct Chmod<'a> {
    locations: Locations<'a>,
    prefix: Option<&'a str>,
}

/// Text normalized for the word scan, and its words.
struct Prepared {
    normalized: Vec<char>,
    index_map: Vec<usize>,
    words: Vec<ShellWord>,
}

/// Fold continuations, mask redirections, strip escapes, and scan words.
///
/// # Errors
///
/// The nesting refusal for hostile substitution nesting.
fn prepare(text: &[char]) -> Result<Prepared, String> {
    let masked = mask_shell_redirections(&normalize_line_continuations(text), 0)?;
    let (normalized, index_map) = strip_shell_escapes(&masked);
    let words = scan_shell_words(&normalized)?;
    Ok(Prepared {
        normalized,
        index_map,
        words,
    })
}

/// The guard's verdict on `script` (the exact text the shell runs).
pub(crate) fn check(script: &Script<'_>, context: &GuardContext) -> Result<(), String> {
    let chmod = Chmod {
        locations: Locations::new(context),
        prefix: script.prefix,
    };
    let text: Vec<char> = script.script.chars().collect();
    chmod.guard(&text, 0)
}

impl Chmod<'_> {
    /// The whole decision, in the kernel's gate order (the first gate that
    /// fires decides the message).
    #[expect(
        clippy::too_many_lines,
        reason = "the gate order is the contract: one sequence reads clearer than split halves"
    )]
    fn guard(&self, script: &[char], heredoc_depth: usize) -> Result<(), String> {
        if heredoc_depth > MAX_HEREDOC_NESTING {
            return Err(messages::nesting());
        }
        let raw = normalize_line_continuations(script);
        let masked = mask_shell_redirections(&raw, 0)?;
        let (normalized, index_map) = strip_shell_escapes(&masked);
        let words = scan_shell_words(&normalized)?;
        let prepared = Prepared {
            normalized,
            index_map,
            words,
        };
        let (normalized, words) = (&prepared.normalized, &prepared.words);
        let user_command_start = match self.prefix.filter(|prefix| !prefix.is_empty()) {
            Some(prefix) => {
                let prefix_end = prefix.chars().count() + 1;
                prepared
                    .index_map
                    .iter()
                    .position(|original| *original >= prefix_end)
                    .unwrap_or(normalized.len())
            }
            None => 0,
        };
        let text: String = normalized.iter().collect();
        if bash_env_words_arm_shell_code(words) {
            return Err(messages::bash_env());
        }
        let (hash_aliases, hash_unreadable) = hash_registered_command_names(words);
        if hash_unreadable {
            return Err(messages::hash_alias());
        }
        let feeds = normalized
            .windows(2)
            .any(|pair| matches!(pair, ['<' | '>', '(']))
            || normalized
                .windows(3)
                .any(|triple| triple == ['<', '<', '<']);
        if feeds && process_substitution_feeds_wrapper(normalized, words) {
            return Err(messages::process_substitution());
        }
        if raw.windows(2).any(|pair| pair == ['<', '<']) {
            self.heredoc_bodies_hide_shell_code(&raw, heredoc_depth)?;
        }
        let any_word =
            |predicate: &dyn Fn(&str) -> bool| words.iter().any(|word| predicate(&word.value));
        let eval_reason = if any_word(&|value| value == "eval") || has_word(&text, &["eval"]) {
            self.wrapper_payloads_hide_shell_code(normalized, PayloadKind::Eval)?
        } else {
            None
        };
        let shell_c_reason = if any_word(&|value| named(value, &SHELL_C_INTERPRETERS))
            || has_word(&text, &SHELL_C_INTERPRETERS)
        {
            self.wrapper_payloads_hide_shell_code(normalized, PayloadKind::ShellC)?
        } else {
            None
        };
        let alias_reason =
            if any_word(&|value| basename(value) == "alias") || has_word(&text, &["alias"]) {
                self.wrapper_payloads_hide_shell_code(normalized, PayloadKind::Alias)?
            } else {
                None
            };
        if function_definition_could_recurse(normalized, words) {
            return Err(messages::definition());
        }
        if shell_wrapper_reads_pipe(normalized, words) {
            return Err(messages::pipe_fed_wrapper());
        }
        let env_s_reason =
            if any_word(&|value| basename(value) == "env") || has_word(&text, &["env"]) {
                self.env_split_payloads_hide_shell_code(normalized)?
            } else {
                None
            };
        match env_s_reason {
            Some(PayloadReason::RecursiveChmod) => return Err(messages::env_split_string()),
            Some(reason) => return Err(messages::payload_reason(reason)),
            None => {}
        }
        let trap_reason = if any_word(&|value| value == "trap") {
            self.wrapper_payloads_hide_shell_code(normalized, PayloadKind::Trap)?
        } else {
            None
        };
        match trap_reason {
            Some(PayloadReason::RecursiveChmod) => return Err(messages::trap()),
            Some(reason) => return Err(messages::payload_reason(reason)),
            None => {}
        }
        if eval_reason == Some(PayloadReason::RecursiveChmod) {
            return Err(messages::eval());
        }
        if shell_c_reason == Some(PayloadReason::RecursiveChmod) {
            return Err(messages::shell_c());
        }
        if alias_reason.is_some() {
            return Err(messages::definition());
        }
        if let Some(reason) = eval_reason.or(shell_c_reason) {
            return Err(messages::payload_reason(reason));
        }
        if any_word(&|value| named(value, &SCRIPT_INPUT_WRAPPERS)) {
            match self.unscanned_wrapper_script_reason(
                &raw,
                normalized,
                words,
                user_command_start,
            )? {
                Some(ScriptReason::Relocation) => return Err(messages::relocation()),
                Some(ScriptReason::Startup) => return Err(messages::shell_startup()),
                Some(ScriptReason::UnscannedScript | ScriptReason::Other) => {
                    return Err(messages::wrapper_script())
                }
                None => {}
            }
        }
        if self.unresolvable_words_could_recurse(words, normalized, true) {
            return Err(messages::unresolvable_command());
        }
        let invocations = find_invocations(words, Some(&hash_aliases));
        if invocations.is_empty() {
            return Ok(());
        }
        if self.prefix_relocates()? {
            return Err(messages::relocation());
        }
        let locations = &self.locations;
        let shadows_command_lookup = path_can_shadow_command_lookup(locations, words);
        for invocation in invocations {
            let invocation_word = &words[invocation.word_index].value;
            if shadows_command_lookup
                && is_chmod_chown_word(invocation_word)
                && !invocation_word.contains('/')
            {
                return Err(messages::shadowed_command());
            }
            let mut run_words: Vec<String> = Vec::new();
            for earlier in words[..invocation.word_index].iter().rev() {
                run_words.push(earlier.value.clone());
                if earlier.starts_command {
                    break;
                }
            }
            run_words.reverse();
            for (wrapper, tokens) in wrapper_chain_groups(&run_words) {
                let relocates = match wrapper.as_str() {
                    "xargs" => true,
                    "env" => !env_option_values(&tokens, 'C', "chdir").is_empty(),
                    "find" => tokens.iter().any(|token| token == "-execdir"),
                    _ => false,
                };
                if relocates {
                    return Err(messages::relocation());
                }
            }
            let base = match effective_cwd(
                locations,
                py_slice(normalized, 0, invocation.start),
                user_command_start,
            ) {
                EffectiveCwd::Unresolvable => return Err(messages::relocation()),
                EffectiveCwd::Workspace => locations.workspace.clone(),
                EffectiveCwd::Dir(dir) => dir,
            };
            let (region, well_formed) =
                region_words(py_slice(normalized, invocation.start, invocation.end));
            for operand in operand_words(&region, well_formed) {
                let resolved = operand
                    .as_deref()
                    .and_then(|operand| resolve_operand(locations, operand, &base));
                if let Some(reason) = operand_violation(locations, resolved.as_deref()) {
                    return Err(messages::operand(
                        operand.as_deref(),
                        resolved.as_deref(),
                        &locations.workspace,
                        reason,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Here-document bodies that execute as shell code (fed to a shell
    /// wrapper, or flowing out of a substitution) get the full guard, with
    /// the trusted prefix replayed in front.
    fn heredoc_bodies_hide_shell_code(
        &self,
        raw: &[char],
        heredoc_depth: usize,
    ) -> Result<(), String> {
        let substitutions = substitution_spans(raw);
        for operator in redirect_operators(raw) {
            if operator.is_here_string(raw) || !operator.is_heredoc(raw) {
                continue;
            }
            let heredoc = locate_heredoc(raw, operator);
            let (Some(_), Some(body_end)) = (&heredoc.delimiter, heredoc.body_end) else {
                continue;
            };
            let body: String = py_slice(raw, heredoc.delimiter_end, body_end)
                .iter()
                .collect();
            if strip(&body).is_empty() {
                continue;
            }
            let reader_is_wrapper = word_before(raw, operator.start, true)
                .is_some_and(|reader| named(&reader, &SHELL_C_INTERPRETERS));
            let inside_substitution = substitutions
                .iter()
                .any(|(start, end)| *start < operator.start && operator.start < *end);
            if reader_is_wrapper || inside_substitution {
                let body = body.trim_matches('\n');
                let rescan = match self.prefix.filter(|prefix| !prefix.is_empty()) {
                    Some(prefix) => format!("{prefix}\n{body}"),
                    None => body.to_string(),
                };
                let rescan: Vec<char> = rescan.chars().collect();
                self.guard(&rescan, heredoc_depth + 1)?;
            }
        }
        Ok(())
    }
}
