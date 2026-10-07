//! The target rules: whether one guarded push would rewrite a protected ref
//! (main/master, `@{u}`, every branch, or the current upstream).

use std::time::Duration;

use crate::context::GuardContext;

use super::budget::{Budget, Scan};
use super::config::cd_environment;
use super::cwd::{resolve_push_cwd, PushCwd};
use super::messages;
use super::push::{PushArgs, PushRun};
use super::text::{has_glob_or_substitution, slice};
use super::upstream::{probe_upstream, ProbeCache, ProbeTimedOut, UpstreamInfo};
use super::words::ShellWord;

const PROTECTED_BRANCHES: [&str; 2] = ["main", "master"];

/// What one check knows about where its pushes run.
pub(super) struct PushSite<'a> {
    pub words: &'a [ShellWord],
    /// The scanned text (escapes folded) the word spans index into.
    pub normalized: &'a [char],
    /// Where the model's text starts after the configured prefix.
    pub user_command_start: usize,
    pub kernel_cwd: &'a str,
    /// The configured prefix relocates (a cd or a `GIT_*=` assignment).
    pub relocating_prefix: bool,
    pub context: &'a GuardContext,
    pub probe_timeout: Duration,
}

/// Probe the upstream of the directory the push at `git_start` runs in, or
/// the refusal that ends the check.
fn probe_push_cwd(
    site: &PushSite<'_>,
    git_start: usize,
    run: &PushRun,
    cache: &mut ProbeCache,
    budget: &Budget,
) -> Scan<Result<Option<UpstreamInfo>, String>> {
    let env_limit = (0..run.git_index)
        .rev()
        .find(|index| site.words[*index].starts_command)
        .unwrap_or(run.git_index);
    let command_env = cd_environment(site.words, env_limit);
    let prefix = slice(site.normalized, 0, git_start);
    let cwd = match resolve_push_cwd(
        &prefix,
        site.user_command_start,
        site.kernel_cwd,
        &command_env,
        site.context,
        budget,
    )? {
        PushCwd::Unresolvable => return Ok(Err(messages::relocation_refusal())),
        PushCwd::Workspace => site.kernel_cwd.to_string(),
        PushCwd::Moved(cwd) => cwd,
    };
    Ok(
        probe_upstream(site.context, &cwd, cache, site.probe_timeout)
            .map_err(|ProbeTimedOut| messages::probe_timeout_refusal()),
    )
}

/// Why this force push must be refused, or `None` when it may run.
pub(super) fn push_violation(
    run: &PushRun,
    args: &PushArgs,
    site: &PushSite<'_>,
    cache: &mut ProbeCache,
    budget: &Budget,
) -> Scan<Option<String>> {
    if run.unresolvable_alias {
        return Ok(Some(messages::alias_refusal()));
    }
    let force = args.force || args.refspecs.iter().any(|spec| spec.starts_with('+'));
    if let Some(word) = &args.unresolvable {
        // Before the dry-run check: an expansion can add `--no-dry-run`.
        return Ok(Some(messages::refusal(&format!(
            "the push argument \"{word}\" cannot be verified statically: the shell may expand it into a force \
             flag or into a refspec naming a protected branch before git reads argv"
        ))));
    }
    if args.dry_run || !force {
        return Ok(None);
    }
    let git_start = site.words[run.git_index].start;
    let in_prefix = git_start < site.user_command_start;
    if args.wildcard {
        return Ok(Some(messages::refusal(
            "a force flag with --all/--mirror rewrites every branch, including main/master and the current upstream",
        )));
    }
    if run.xargs_fed {
        return Ok(Some(messages::refusal(
            "xargs feeds it refspecs from stdin the guard cannot see",
        )));
    }
    if !args.refspecs.is_empty() {
        return explicit_target_violation(run, &args.refspecs, site, cache, budget);
    }
    // Implicit refspec: push.default makes the current upstream the target.
    if in_prefix {
        return Ok(Some(messages::refusal(
            "the configured command prefix force-pushes without a refspec, so the target cannot be verified",
        )));
    }
    if run.relocated || site.relocating_prefix {
        return Ok(Some(messages::relocation_refusal()));
    }
    let probed = match probe_push_cwd(site, git_start, run, cache, budget)? {
        Ok(probed) => probed,
        Err(refusal) => return Ok(Some(refusal)),
    };
    let Some(info) = probed else {
        return Ok(None); // not a repository: git errors on its own
    };
    Ok(Some(match info.upstream_ref {
        None => messages::refusal(&format!(
            "without a refspec, and with no upstream on the current branch \"{}\", the push target comes from \
             push.default, remote.<name>.push, or remote.<name>.mirror configuration the guard cannot read",
            info.current_branch
        )),
        Some(upstream) => messages::refusal(&format!(
            "without a refspec it would force-push the current branch onto its upstream \"{upstream}\""
        )),
    }))
}

/// The target rules for explicit refspecs: a protected destination, an
/// unverifiable one, the upstream, or `HEAD` naming a protected branch.
fn explicit_target_violation(
    run: &PushRun,
    refspecs: &[String],
    site: &PushSite<'_>,
    cache: &mut ProbeCache,
    budget: &Budget,
) -> Scan<Option<String>> {
    let git_start = site.words[run.git_index].start;
    let in_prefix = git_start < site.user_command_start;
    for refspec in refspecs {
        let body = refspec.strip_prefix('+').unwrap_or(refspec);
        let target = match body.split_once(':') {
            Some(("", "")) => {
                return Ok(Some(messages::refusal(
                    "the refspec \":\" deletes every branch on the remote",
                )));
            }
            // The remote side is the ref being rewritten (`:dst` deletes
            // it); `src:` deletes src, conservatively protected.
            Some((src, "")) => src,
            Some((_, dst)) => dst,
            None => body,
        };
        if target.is_empty() {
            continue;
        }
        if has_glob_or_substitution(target) {
            return Ok(Some(messages::refusal(&format!(
                "the push target \"{target}\" cannot be verified statically (glob, substitution, or variable)"
            ))));
        }
        if target.starts_with("@{") {
            return Ok(Some(messages::refusal(&format!(
                "the refspec \"{refspec}\" names the current upstream"
            ))));
        }
        let target = target
            .strip_prefix("refs/heads/")
            .or_else(|| target.strip_prefix("heads/"))
            .unwrap_or(target);
        if PROTECTED_BRANCHES.contains(&target) {
            return Ok(Some(messages::refusal(&format!(
                "it would force-push \"{target}\""
            ))));
        }
        if target == "HEAD" || target == "@" {
            if run.relocated || in_prefix || site.relocating_prefix {
                return Ok(Some(messages::relocation_refusal()));
            }
            let probed = match probe_push_cwd(site, git_start, run, cache, budget)? {
                Ok(probed) => probed,
                Err(refusal) => return Ok(Some(refusal)),
            };
            if let Some(info) =
                probed.filter(|info| PROTECTED_BRANCHES.contains(&info.current_branch.as_str()))
            {
                return Ok(Some(messages::refusal(&format!(
                    "HEAD names the current branch \"{}\"",
                    info.current_branch
                ))));
            }
        }
    }
    Ok(None)
}
