//! Destructive-git dirty-tree guard: a git command that discards uncommitted
//! working-tree changes (`git checkout -- .`, `git restore .`, `git reset
//! --hard`, `git clean` that is not a dry run) is refused while the
//! repository it targets has uncommitted changes. The pattern scan is
//! text-only; the `git status` probe runs only on a match, in the repository
//! the discard targets (cd chains, `git -C`, `GIT_DIR`-style assignments are
//! replayed), and the guard refuses outright what it cannot replay (an `eval`
//! payload, a relocation it cannot follow, a revealed command value).
//!
//! The taxonomy and bypass semantics follow the coding-agent bash tool's
//! guard, hardened for eval payloads, attached short options, line
//! continuations and redirections.

mod eval;
mod messages;
mod sites;
mod target;
mod text;
mod words;

use std::time::Duration;

use crate::context::GuardContext;
use crate::probe::{run_probe, ProbeLimits, ProbeOutcome};
use crate::script::Script;
use crate::verdict::GuardKind;

use messages::Refusal;
use target::{ProbeTarget, GIT_STATUS_PORCELAIN_COMMAND};
use words::Names;

/// The git guard has no warn-once: its late-bypass note rides in the message.
pub(crate) const LATE_BYPASS_WARNING: Option<&str> = None;

/// Whether this guard's port is complete.
#[cfg(test)]
pub(crate) const PORTED: bool = true;

/// The probe is read-only, but a wedged git must not wedge the kernel; the
/// parsed listing is bounded, and dirtiness past the cap still refuses.
const PROBE_LIMITS: ProbeLimits = ProbeLimits {
    timeout: Duration::from_secs(10),
    kill_grace: Duration::from_secs(1),
    output_cap: Some(64 * 1024),
};

/// Refuse a destructive discard while the tree it targets is dirty.
///
/// # Errors
///
/// The refusal message.
pub(crate) fn check(script: &Script<'_>, context: &GuardContext) -> Result<(), String> {
    let late_bypass = context.late_bypass(GuardKind::DestructiveGit);
    for probe in planned_probes(script, late_bypass)? {
        if let Some(dirty_paths) = probe_uncommitted_changes(context, &probe.command, PROBE_LIMITS)
        {
            if !dirty_paths.is_empty() {
                return Err(messages::dirty_tree(
                    &dirty_paths,
                    probe.includes_ignored_files,
                    late_bypass,
                ));
            }
        }
    }
    Ok(())
}

/// One dirty-tree probe the guard must run before the discard may proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Probe {
    /// The shell text: the trusted prefix, the replayed relocation, then the
    /// `git status` listing.
    command: String,
    /// The discard deletes ignored files too, so the listing includes them.
    includes_ignored_files: bool,
}

/// The probes `script` needs (none when it holds no discard), or the
/// refusal when a discard cannot be probed safely.
fn planned_probes(script: &Script<'_>, late_bypass: bool) -> Result<Vec<Probe>, String> {
    let resolved = text::mask_shell_redirections(&text::normalize_line_continuations(
        &text::chars(script.script),
    ));
    // An escaped or split-spelled `eval` (`e\val`, `e'va'l`) still runs the
    // builtin, so the gate reads the text with quoting and escapes dropped.
    let ungated: Vec<char> = resolved
        .iter()
        .copied()
        .filter(|ch| !"\"'\\".contains(*ch))
        .collect();
    let eval_present = text::contains(&ungated, "eval");
    let none = Names::new();
    if eval_present && eval::payloads_hide_discard(&resolved, 0, &none, &none) {
        return Err(messages::refusal(Refusal::Eval, late_bypass));
    }
    let sites = sites::find_discard_sites(&resolved, &none, &none);
    if sites.is_empty() {
        return Ok(Vec::new());
    }
    if eval_present && eval::payloads_relocate(&resolved) {
        return Err(messages::refusal(Refusal::Relocation, late_bypass));
    }
    let prefix = script.prefix.filter(|prefix| !prefix.is_empty());
    let user_command_start = prefix.map_or(0, |prefix| prefix.chars().count() + 1);
    let mut probes: Vec<Probe> = Vec::new();
    for site in sites {
        if site.revealed {
            return Err(messages::refusal(Refusal::RevealedCommand, late_bypass));
        }
        let (relocation_prefix, git_status) =
            match target::resolve_probe_target(&resolved, site.index, user_command_start) {
                ProbeTarget::Unresolvable => {
                    return Err(messages::refusal(Refusal::Relocation, late_bypass))
                }
                ProbeTarget::Caller => (String::new(), GIT_STATUS_PORCELAIN_COMMAND.to_string()),
                ProbeTarget::Relocated {
                    relocation_prefix,
                    git_status_command,
                } => (relocation_prefix.unwrap_or_default(), git_status_command),
            };
        let command = Script::compose(&format!("{relocation_prefix}{git_status}"), prefix);
        if probes.iter().all(|probe| probe.command != command) {
            probes.push(Probe {
                command,
                includes_ignored_files: git_status.contains("--ignored=matching"),
            });
        }
    }
    Ok(probes)
}

/// The at-risk paths `probe_command` lists in the kernel's directory, or
/// `None` when dirtiness cannot be determined (not a repository, git missing,
/// probe failure, timeout): the guard then fails open instead of blocking on
/// a guess. A listing cut at the cap still proves dirtiness.
fn probe_uncommitted_changes(
    context: &GuardContext,
    probe_command: &str,
    limits: ProbeLimits,
) -> Option<Vec<String>> {
    let ProbeOutcome::Finished {
        status,
        stdout,
        truncated,
    } = run_probe(context, probe_command, context.cwd(), limits)
    else {
        return None;
    };
    if status != Some(0) && !truncated {
        return None;
    }
    let mut listing = String::from_utf8_lossy(&stdout).into_owned();
    if truncated {
        if let Some(cut) = listing.rfind('\n') {
            listing.truncate(cut);
        }
    }
    Some(
        listing
            .split('\n')
            .filter(|line| !text::trim(&text::chars(line)).is_empty())
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
            .collect(),
    )
}

#[cfg(test)]
mod tests;
