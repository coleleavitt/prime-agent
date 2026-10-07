//! The privilege-escalation guard: a command that invokes sudo or doas is
//! refused before any process starts, because root escapes the containment
//! every other guard relies on.
//!
//! The scan is string-only: a shell lexer that folds quotes, escapes and
//! ANSI-C text the shell's way ([`lexer`]), a walk over every command word
//! and every payload the text runs ([`scan`]), and the reading of a word as a
//! name the shell could resolve to sudo/doas: basename, folded case, brace
//! alternatives and globs ([`names`], [`glob`]), against the wrapper and
//! launcher taxonomy in [`tables`].

mod glob;
mod lexer;
mod names;
mod scan;
mod tables;

use crate::context::GuardContext;
use crate::script::Script;

/// The stderr warning printed (once) when the bypass variable appeared after
/// kernel start.
pub(crate) const LATE_BYPASS_WARNING: Option<&str> = Some(
    "prime-agent bash: PI_BASH_ALLOW_SUDO appeared after kernel start and is ignored; the sudo \
     guard only honors it when the kernel is started with it set.",
);

/// Whether this guard's port is complete.
#[cfg(test)]
pub(crate) const PORTED: bool = true;

/// The reason phrase when the text invokes sudo/doas as a command.
fn violation(command: &str) -> Option<scan::Violation> {
    scan::scan_text(&lexer::join_line_continuations(command), 0, false)
}

/// Refuse a script that would run sudo or doas.
pub(crate) fn check(script: &Script<'_>, _context: &GuardContext) -> Result<(), String> {
    match violation(script.script) {
        None => Ok(()),
        Some(violation) => Err(format!(
            "Refusing to run this command: {violation}. sudo and doas run the command as root \
             (or another user), which escapes the containment every other guard relies on; on a \
             passwordless-sudo setup the escalation is silent. Bypass deliberately, so the intent \
             stays visible in the transcript: call bash(command, allow_sudo=True), or start the \
             kernel with PI_BASH_ALLOW_SUDO=1."
        )),
    }
}

#[cfg(test)]
mod tests;
