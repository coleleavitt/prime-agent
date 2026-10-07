//! Pipe-to-shell guard: refuses a curl/wget download that a shell
//! interpreter would run, piped into one (`curl URL | sh`) or substituted
//! into its argv or script (`sh -c "$(curl URL)"`, `bash <(curl URL)`).
//!
//! Detection is text-only: a quote-aware split into pipeline stages
//! ([`region`]), the command word each stage runs behind its wrappers
//! ([`stage`]), and a fail-closed reading of what each stage feeds
//! ([`violation`]). No URL is fetched and no process starts. Deliberately
//! allowed: downloads to a file or a redirect, downloads read downstream by a
//! non-runner, a plain `sh script.sh`, and the download-then-run sequence.

mod region;
mod stage;
mod violation;

#[cfg(test)]
mod tests;

use crate::context::GuardContext;
use crate::script::Script;

/// The stderr warning printed (once) when the bypass variable appeared after
/// kernel start.
pub(crate) const LATE_BYPASS_WARNING: Option<&str> = Some(
    "prime-agent bash: PI_BASH_ALLOW_PIPE_TO_SHELL appeared after kernel start and is ignored; \
     the pipe-to-shell guard only honors it when the kernel is started with it set.",
);

/// Whether this guard's port is complete.
#[cfg(test)]
pub(crate) const PORTED: bool = true;

/// Refuse a download a shell interpreter would run.
///
/// # Errors
///
/// The refusal message, naming the violation.
pub(crate) fn check(script: &Script<'_>, _context: &GuardContext) -> Result<(), String> {
    match violation::text_violation(script.script, 0) {
        None => Ok(()),
        Some(found) => Err(refusal_message(found.phrase())),
    }
}

fn refusal_message(violation: &str) -> String {
    [
        "Refusing to run this command: piping or substituting curl/wget",
        "output into a shell interpreter downloads and executes remote code",
        &format!("without review ({violation})."),
        "",
        "Download the script to a file, read the file, then run it in a",
        "later command (curl -o script.sh URL, then sh script.sh).",
        "",
        "If the download is trusted, retry with",
        "bash(command, allow_pipe_to_shell=True), or start the kernel with",
        "PI_BASH_ALLOW_PIPE_TO_SHELL=1; the variable is frozen at kernel start,",
        "so writing it mid-session never unlocks the guard.",
    ]
    .join("\n")
}
