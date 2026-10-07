//! The secret-echo guard: refuse a command whose output would put a whole
//! environment or a known secret file into the transcript, where it persists
//! in session logs that models and users read later.
//!
//! Detection reads the command text only (no filesystem access): two
//! length-preserving masks of the literal spans, a segment split, a
//! here-document line pass, a shell-faithful word split of each segment, and
//! a walk over every command substitution's interior (from a worklist, so
//! nesting depth costs time rather than stack). The rules:
//!
//! - a segment whose command word is `env`/`printenv` with only flags after
//!   it, or `export` with no names, dumps the environment (leading `FOO=1`
//!   words and redirections dropped, quoted command words built the shell's
//!   way, `env -S` operands read as the command line they are);
//! - such a dump piped into `grep` for one fixed string is the targeted read
//!   the refusal suggests, allowed while the dump really feeds the pipe;
//! - a `cat`/`echo` (also run through `env`) naming `~/.ssh`, `~/.gnupg` or
//!   `~/.aws` in an expanding spelling reads a secret file;
//! - here-document bodies are never shell input; unquoted ones still expand
//!   command substitutions, which the walk reads.

mod dump;
mod heredoc;
mod mask;
mod secret_path;
mod substitutions;
mod words;

#[cfg(test)]
mod tests;

use crate::context::GuardContext;
use crate::script::Script;

use dump::{is_bare_dump, is_bounded_grep_filter, leaves_the_pipe, pipe_follower_words};
use heredoc::heredoc_segments;
use mask::{blank_segments, command_segments, mask_literals, DoubleQuotes};
use secret_path::{
    executor_reader, home_var_secret_path, live_secret_path_word, split_operand_secret_path,
    tilde_secret_path, SECRET_READ_COMMANDS,
};
use substitutions::{command_substitutions, Descend};
use words::analysis_words;

/// The stderr warning printed (once) when the bypass variable appeared after
/// kernel start.
pub(crate) const LATE_BYPASS_WARNING: Option<&str> = Some(
    "prime-agent bash: PI_BASH_ALLOW_SECRET_ECHO appeared after kernel start and is ignored; \
     the secret-echo guard only honors it when the kernel is started with it set.",
);

/// Whether this guard's port is complete.
#[cfg(test)]
pub(crate) const PORTED: bool = true;

/// What a refused command would print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Violation {
    FullEnvironment,
    SecretFile,
}

impl Violation {
    fn phrase(self) -> &'static str {
        match self {
            Violation::FullEnvironment => "the full environment",
            Violation::SecretFile => "a known secret file",
        }
    }
}

/// Refuse a command that would echo secrets into the transcript.
pub(crate) fn check(script: &Script<'_>, _context: &GuardContext) -> Result<(), String> {
    match violation(script.script) {
        None => Ok(()),
        Some(violation) => Err(refusal_message(violation)),
    }
}

fn refusal_message(violation: Violation) -> String {
    let phrase = violation.phrase();
    [
        format!("Refusing to run this command: it would print {phrase} into").as_str(),
        "the transcript, where the output persists in session logs that",
        "models and users read later.",
        "",
        "Read only what you need instead: printenv SAFE_VAR for a single",
        "variable, env | grep SAFE_VAR to filter a dump, or grep KEY",
        "<file> for one key out of a file.",
        "",
        "If the full output is intentional, retry with",
        "bash(command, allow_secret_echo=True), or start the kernel with",
        "PI_BASH_ALLOW_SECRET_ECHO=1.",
    ]
    .join("\n")
}

/// Why `command` would echo secrets into the transcript, if it would.
fn violation(command: &str) -> Option<Violation> {
    let mut pending: Vec<(Vec<char>, Descend)> = vec![(command.chars().collect(), Descend::Yes)];
    while let Some((current, descend)) = pending.pop() {
        let literal = mask_literals(&current, DoubleQuotes::Masked);
        let expanded = mask_literals(&current, DoubleQuotes::Live);
        let segments = command_segments(&literal);
        let heredocs = heredoc_segments(&current, &literal, &segments);
        for (index, segment) in segments.iter().enumerate() {
            if heredocs.bodies.contains(&index) {
                continue;
            }
            let words = analysis_words(&current, segment.start, segment.end);
            if words.is_empty() {
                continue;
            }
            if is_bare_dump(&words) {
                // `env | grep SAFE_VAR` stays allowed, but only while the
                // dump really feeds the pipe.
                let filtered = segment.separator == Some('|')
                    && index + 1 < segments.len()
                    && !leaves_the_pipe(&literal[segment.start..segment.end])
                    && is_bounded_grep_filter(&pipe_follower_words(&current, &segments, index + 1));
                if filtered {
                    continue;
                }
                return Some(Violation::FullEnvironment);
            }
            let reader =
                SECRET_READ_COMMANDS.contains(&words[0].as_str()) || executor_reader(&words);
            if reader
                && (tilde_secret_path(&literal[segment.start..segment.end])
                    || home_var_secret_path(&expanded[segment.start..segment.end])
                    || live_secret_path_word(
                        &current,
                        &literal,
                        &expanded,
                        segment.start,
                        segment.end,
                    )
                    || split_operand_secret_path(&words))
            {
                return Some(Violation::SecretFile);
            }
        }
        // A quoted body prints its text rather than running it, and a closing
        // line ends a body rather than feeding anything: both are blanked
        // before the walk; an unquoted body still expands.
        let runnable = blank_segments(
            &current,
            &segments,
            heredocs
                .quoted_bodies
                .union(&heredocs.delimiter_lines)
                .copied(),
        );
        pending.extend(command_substitutions(&runnable, descend));
    }
    None
}
