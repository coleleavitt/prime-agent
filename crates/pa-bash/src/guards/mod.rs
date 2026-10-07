//! The six refusal guards. Each reads the script text (plus, where it must,
//! the filesystem or a read-only git probe) and refuses with its own message.

use crate::context::GuardContext;
use crate::script::Script;
use crate::verdict::{GuardKind, Refusal};

mod destructive_chmod;
mod destructive_git;
mod force_push;
mod pipe_to_shell;
mod secret_echo;
mod sudo;

#[cfg(test)]
mod corpus_tests;

/// One guard's verdict on `script`.
///
/// # Errors
///
/// The guard's [`Refusal`].
pub(crate) fn check(
    guard: GuardKind,
    script: &Script<'_>,
    context: &GuardContext,
) -> Result<(), Refusal> {
    let (verdict, warning) = match guard {
        GuardKind::DestructiveGit => (
            destructive_git::check(script, context),
            destructive_git::LATE_BYPASS_WARNING,
        ),
        GuardKind::DestructiveChmod => (
            destructive_chmod::check(script, context),
            destructive_chmod::LATE_BYPASS_WARNING,
        ),
        GuardKind::ForcePush => (
            force_push::check(script, context),
            force_push::LATE_BYPASS_WARNING,
        ),
        GuardKind::SecretEcho => (
            secret_echo::check(script, context),
            secret_echo::LATE_BYPASS_WARNING,
        ),
        GuardKind::PipeToShell => (
            pipe_to_shell::check(script, context),
            pipe_to_shell::LATE_BYPASS_WARNING,
        ),
        GuardKind::Sudo => (sudo::check(script, context), sudo::LATE_BYPASS_WARNING),
    };
    verdict.map_err(|message| Refusal {
        guard,
        message,
        late_bypass_warning: warning
            .filter(|_| context.late_bypass(guard))
            .map(str::to_string),
    })
}

/// Whether a guard's port is complete (the corpus harness checks it).
#[cfg(test)]
pub(crate) fn ported(guard: GuardKind) -> bool {
    match guard {
        GuardKind::DestructiveGit => destructive_git::PORTED,
        GuardKind::DestructiveChmod => destructive_chmod::PORTED,
        GuardKind::ForcePush => force_push::PORTED,
        GuardKind::SecretEcho => secret_echo::PORTED,
        GuardKind::PipeToShell => pipe_to_shell::PORTED,
        GuardKind::Sudo => sudo::PORTED,
    }
}
