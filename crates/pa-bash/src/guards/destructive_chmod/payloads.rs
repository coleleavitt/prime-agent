//! Shell code the plain scan cannot see: quoted payloads handed to wrappers
//! (`eval`, `sh -c`, `alias`, `trap`, `env -S`) are unquoted one layer at a
//! time and rescanned, and the word-level signs that a command name or the
//! file bash runs first could hide a recursive chmod/chown.

use super::invocations::{env_split_string_feeds, find_invocations, hash_registered_command_names};
use super::location::expanded_command_word_value;
use super::messages::PayloadReason;
use super::normalize::{expand_ansi_c_payloads, unquote_one_level};
use super::patterns::{
    assigns, has_expandable_glob, has_function_definition, has_unresolved_expansion,
    is_assignment_word,
};
use super::pyos::basename;
use super::vocabulary::{
    is_chmod_chown_word, is_recursive_token_run, is_short_option, named, ENV_ARMING_HEADS,
    SCRIPT_INPUT_WRAPPERS, SHELL_C_INTERPRETERS, UNRESOLVABLE_COMMAND_EXECUTORS,
};
use super::words::{py_slice, run_followers, run_tokens_from, ShellWord};
use super::{Chmod, ScriptReason};

/// How deep quoted wrappers are unwrapped before the guard refuses outright.
const MAX_EVAL_SCAN_DEPTH: usize = 10;

/// The wrappers whose payload is shell code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PayloadKind {
    Eval,
    ShellC,
    Alias,
    Trap,
}

const ALL_KINDS: [PayloadKind; 4] = [
    PayloadKind::Eval,
    PayloadKind::ShellC,
    PayloadKind::Alias,
    PayloadKind::Trap,
];

/// The raw payload sources handed to wrapper words: the words after each
/// `eval`/`alias`/`trap` (joined with spaces), or the quoted (or
/// substitution) payload word after the `-c` cluster of a shell wrapper.
/// `text` is the string the word spans index into.
pub(super) fn wrapper_payload_sources(
    words: &[ShellWord],
    text: &[char],
    kinds: &[PayloadKind],
) -> Vec<Vec<char>> {
    let mut sources = Vec::new();
    for (index, word) in words.iter().enumerate() {
        let kind = if kinds.contains(&PayloadKind::Eval) && word.value == "eval" {
            Some(PayloadKind::Eval)
        } else if kinds.contains(&PayloadKind::ShellC) && named(&word.value, &SHELL_C_INTERPRETERS)
        {
            Some(PayloadKind::ShellC)
        } else if kinds.contains(&PayloadKind::Alias) && basename(&word.value) == "alias" {
            Some(PayloadKind::Alias)
        } else if kinds.contains(&PayloadKind::Trap) && word.value == "trap" {
            Some(PayloadKind::Trap)
        } else {
            None
        };
        match kind {
            None => {}
            Some(PayloadKind::ShellC) => {
                let mut c_pending = false;
                for follower in run_followers(words, index) {
                    let token = follower.value.as_str();
                    if c_pending {
                        if follower.contained {
                            continue;
                        }
                        let source = py_slice(text, follower.start, follower.end);
                        let opens_payload = matches!(source.first(), Some('\'' | '"' | '`'))
                            || matches!(source, ['$', '\'' | '"' | '(', ..]);
                        if opens_payload {
                            sources.push(source.to_vec());
                        }
                        break;
                    }
                    if token == "--" {
                        break;
                    }
                    if is_short_option(token) && token[1..].contains('c') {
                        c_pending = true;
                    }
                }
            }
            Some(PayloadKind::Eval | PayloadKind::Alias | PayloadKind::Trap) => {
                let parts: Vec<&[char]> = run_followers(words, index)
                    .map(|follower| py_slice(text, follower.start, follower.end))
                    .collect();
                if !parts.is_empty() {
                    sources.push(parts.join(&' '));
                }
            }
        }
    }
    sources
}

/// One payload source unquoted one layer, ANSI-C and locale quoting folded.
fn unwrap_source(source: &[char]) -> Vec<char> {
    unquote_one_level(&expand_ansi_c_payloads(source))
}

impl Chmod<'_> {
    /// Why a one-level-unquoted wrapper payload hides shell code the guard
    /// must refuse, recursing into further wrappers inside it.
    pub(super) fn payload_hides_shell_code(
        &self,
        text: &[char],
        depth: usize,
    ) -> Result<Option<PayloadReason>, String> {
        if depth > MAX_EVAL_SCAN_DEPTH {
            return Ok(Some(PayloadReason::RecursiveChmod));
        }
        let prepared = super::prepare(text)?;
        let (normalized, words) = (&prepared.normalized, &prepared.words);
        let (hash_aliases, hash_unreadable) = hash_registered_command_names(words);
        if hash_unreadable {
            return Ok(Some(PayloadReason::UnresolvableCommand));
        }
        if !find_invocations(words, Some(&hash_aliases)).is_empty() {
            return Ok(Some(PayloadReason::RecursiveChmod));
        }
        if bash_env_words_arm_shell_code(words) {
            return Ok(Some(PayloadReason::BashEnv));
        }
        if self.unresolvable_words_could_recurse(words, normalized, false) {
            return Ok(Some(PayloadReason::UnresolvableCommand));
        }
        if super::wrappers::process_substitution_feeds_wrapper(normalized, words) {
            return Ok(Some(PayloadReason::ProcessSubstitution));
        }
        let (feeds, expansion) = env_split_string_feeds(words);
        for feed in feeds {
            let feed: Vec<char> = feed.chars().collect();
            if let Some(reason) = self.payload_hides_shell_code(&feed, depth + 1)? {
                return Ok(Some(reason));
            }
        }
        if expansion {
            return Ok(Some(PayloadReason::EnvSplitExpansion));
        }
        if words
            .iter()
            .any(|word| named(&word.value, &SCRIPT_INPUT_WRAPPERS))
        {
            match self.unscanned_wrapper_script_reason(text, normalized, words, 0)? {
                Some(ScriptReason::Startup) => return Ok(Some(PayloadReason::ShellStartup)),
                Some(_) => return Ok(Some(PayloadReason::UnscannedScript)),
                None => {}
            }
        }
        for source in wrapper_payload_sources(words, normalized, &ALL_KINDS) {
            if let Some(reason) =
                self.payload_hides_shell_code(&unwrap_source(&source), depth + 1)?
            {
                return Ok(Some(reason));
            }
        }
        Ok(None)
    }

    /// Why a quoted payload of one wrapper kind in `command` hides shell code.
    pub(super) fn wrapper_payloads_hide_shell_code(
        &self,
        command: &[char],
        kind: PayloadKind,
    ) -> Result<Option<PayloadReason>, String> {
        let words = super::words::scan_shell_words(command)?;
        for source in wrapper_payload_sources(&words, command, &[kind]) {
            if let Some(reason) = self.payload_hides_shell_code(&unwrap_source(&source), 0)? {
                return Ok(Some(reason));
            }
        }
        Ok(None)
    }

    /// Why a GNU `env -S` string in `command` hides shell code: every split
    /// argv is scanned like a payload; a string carrying expansion is
    /// reported.
    pub(super) fn env_split_payloads_hide_shell_code(
        &self,
        command: &[char],
    ) -> Result<Option<PayloadReason>, String> {
        let words = super::words::scan_shell_words(command)?;
        let (feeds, expansion) = env_split_string_feeds(&words);
        for feed in feeds {
            let feed: Vec<char> = feed.chars().collect();
            if let Some(reason) = self.payload_hides_shell_code(&feed, 0)? {
                return Ok(Some(reason));
            }
        }
        Ok(expansion.then_some(PayloadReason::EnvSplitExpansion))
    }

    /// Whether a word may not be the literal the scanner folded: after the
    /// known `$HOME`/`$PWD` expansions it still carries `$`, a backtick, or
    /// (in a longer word) a glob or brace, or its raw span holds a
    /// substitution.
    fn word_could_expand(&self, word: &ShellWord, span_source: &[char]) -> bool {
        let expanded = expanded_command_word_value(&self.locations, &word.value);
        if has_unresolved_expansion(&expanded) {
            return true;
        }
        if expanded.chars().count() > 1 && has_expandable_glob(&expanded) {
            return true;
        }
        span_source.contains(&'`') || span_source.windows(2).any(|pair| pair == ['$', '('])
    }

    /// Whether a word the scanner cannot resolve, in command position or
    /// inside a run of command-executing wrappers, could expand into a
    /// (recursive, when `require_recursive_flag`) chmod/chown.
    pub(super) fn unresolvable_words_could_recurse(
        &self,
        words: &[ShellWord],
        normalized: &[char],
        require_recursive_flag: bool,
    ) -> bool {
        let mut head: Option<&ShellWord> = None;
        for (index, word) in words.iter().enumerate() {
            if word.heads_run() {
                head = Some(word);
            }
            if is_assignment_word(&word.value) {
                continue;
            }
            if !self.word_could_expand(word, py_slice(normalized, word.start, word.end)) {
                continue;
            }
            let executor_run = head.is_some_and(|head| {
                is_assignment_word(&head.value)
                    || named(&head.value, &UNRESOLVABLE_COMMAND_EXECUTORS)
            });
            if !(word.starts_command || executor_run) {
                continue;
            }
            if require_recursive_flag && !is_recursive_token_run(&run_tokens_from(words, index)) {
                continue;
            }
            return true;
        }
        false
    }
}

/// Whether the words arm `BASH_ENV` for a command (in command position, or
/// as an operand of an assignment run or an arming head).
pub(super) fn bash_env_words_arm_shell_code(words: &[ShellWord]) -> bool {
    let mut head: Option<&ShellWord> = None;
    for word in words {
        if word.heads_run() {
            head = Some(word);
        }
        if !assigns(&word.value, "BASH_ENV") {
            continue;
        }
        if word.starts_command
            || head.is_some_and(|head| {
                is_assignment_word(&head.value) || named(&head.value, &ENV_ARMING_HEADS)
            })
        {
            return true;
        }
    }
    false
}

/// Whether a shell function definition could carry a recursive chmod/chown:
/// a chmod/chown word and a recursive flag anywhere, with a definition.
pub(super) fn function_definition_could_recurse(normalized: &[char], words: &[ShellWord]) -> bool {
    has_function_definition(normalized)
        && words.iter().any(|word| is_chmod_chown_word(&word.value))
        && words
            .iter()
            .any(|word| is_recursive_token_run(std::slice::from_ref(&word.value)))
}
