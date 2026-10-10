//! The refusal rules. Each judges the one command model of the script
//! ([`Model`]) and refuses with its own message; none re-reads the text.
//!
//! A rule refuses what the model shows doing its danger (a force push to
//! `main`, a recursive chmod of `~`, a download piped into `sh`), and code the
//! model cannot see ([`crate::model::Opaque`]) only when that code's visible
//! source carries the rule's evidence ([`opaque`]).

use crate::context::GuardContext;
use crate::model::Model;
use crate::script::Script;
use crate::verdict::{GuardKind, Refusal};

mod destructive_chmod;
mod destructive_git;
mod force_push;
mod git;
mod opaque;
mod pipe_to_shell;
mod secret_echo;
mod sudo;

#[cfg(test)]
mod corpus_tests;
#[cfg(test)]
mod tests;

/// What one rule decides about a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allow,
    Refuse(Refusal),
}

/// The inputs every rule reads.
pub(crate) struct Check<'a> {
    pub model: &'a Model,
    pub script: &'a Script<'a>,
    pub context: &'a GuardContext,
}

/// One refusal rule over the command model.
///
/// An implementation reads only the model (plus, where its danger depends on
/// it, the filesystem or a bounded read-only git probe), refuses what the
/// model shows doing its danger, and refuses an opaque node only on its own
/// evidence words. It never panics on any model.
pub(crate) trait Rule {
    const GUARD: GuardKind;
    /// The one-time stderr warning when the guard's bypass variable appeared
    /// after kernel start (`None`: the note rides in the message).
    const LATE_BYPASS_WARNING: Option<&'static str>;

    /// The refusal message, or `None` to allow.
    fn judge(check: &Check<'_>) -> Option<String>;
}

fn verdict<R: Rule>(check: &Check<'_>) -> Verdict {
    match R::judge(check) {
        None => Verdict::Allow,
        Some(message) => Verdict::Refuse(Refusal {
            guard: R::GUARD,
            message,
            late_bypass_warning: R::LATE_BYPASS_WARNING
                .filter(|_| check.context.late_bypass(R::GUARD))
                .map(str::to_string),
        }),
    }
}

/// `guard`'s verdict on the model.
pub(crate) fn judge(guard: GuardKind, check: &Check<'_>) -> Verdict {
    match guard {
        GuardKind::DestructiveGit => verdict::<destructive_git::DestructiveGit>(check),
        GuardKind::DestructiveChmod => verdict::<destructive_chmod::DestructiveChmod>(check),
        GuardKind::ForcePush => verdict::<force_push::ForcePush>(check),
        GuardKind::SecretEcho => verdict::<secret_echo::SecretEcho>(check),
        GuardKind::PipeToShell => verdict::<pipe_to_shell::PipeToShell>(check),
        GuardKind::Sudo => verdict::<sudo::Sudo>(check),
    }
}

/// One guard's verdict on `script` (its own model build).
#[cfg(test)]
pub(crate) fn check(
    guard: GuardKind,
    script: &Script<'_>,
    context: &GuardContext,
) -> Result<(), Refusal> {
    let model = Model::build(script, context);
    match judge(
        guard,
        &Check {
            model: &model,
            script,
            context,
        },
    ) {
        Verdict::Allow => Ok(()),
        Verdict::Refuse(refusal) => Err(refusal),
    }
}

pub(crate) use destructive_git::is_discard_command as is_destructive_git_discard;
