//! Destructive-git rule: a git command that discards uncommitted
//! working-tree changes (`git checkout -- .`, `git checkout -f BRANCH`, `git
//! restore .`, `git reset --hard`, `git clean` that is not a dry run) is
//! refused while the repository it targets has uncommitted changes. The
//! `git status` probe runs only for a discard, in the directory the discard
//! runs in, with its global options (`-C`, `--git-dir`) and assignments
//! (`GIT_DIR=...`) replayed; a discard whose repository the model cannot
//! name (a `cd "$dir"` before it) is refused outright.

use std::collections::BTreeMap;

use super::git::{GitCall, STATUS_LIMITS, directories, git_call, probe};
use super::{Check, Rule, opaque};
use crate::context::GuardContext;
use crate::model::evidence::find_word;
use crate::model::{Arg, Model, Via};
use crate::script::Script;
use crate::verdict::GuardKind;

pub(crate) struct DestructiveGit;

/// How many dirty paths the refusal lists before eliding the rest.
const MAX_DIRTY_PATHS_LISTED: usize = 10;

impl Rule for DestructiveGit {
    const GUARD: GuardKind = GuardKind::DestructiveGit;
    // The note rides in the message.
    const LATE_BYPASS_WARNING: Option<&'static str> = None;

    fn judge(check: &Check<'_>) -> Option<String> {
        let late = check.context.late_bypass(GuardKind::DestructiveGit);
        let mut probed: Vec<String> = Vec::new();
        for invocation in &check.model.invocations {
            let Some(call) = git_call(invocation) else {
                continue;
            };
            let Some(discard) = discard(&call) else {
                continue;
            };
            if invocation
                .context
                .iter()
                .any(|via| matches!(via, Via::Remote { .. }))
            {
                // A remote tree is not the workspace; the guard cannot read it.
                continue;
            }
            let shown = invocation.shown();
            let dirs = match directories(invocation) {
                Ok(dirs) => dirs,
                Err(relocation) => {
                    return Some(refusal(
                        &format!(
                            "it changes directory (or repository) first (`{relocation}` before `{shown}`), and the uncommitted changes of the repository it targets cannot be checked safely."
                        ),
                        "Run the discard as its own command from the target directory, or retry with bash(command, allow_destructive_git=True).",
                        late,
                    ));
                }
            };
            if !call.replayable() || relocates_unreadably(&call) {
                return Some(refusal(
                    &format!(
                        "its repository is chosen by options or variables only known at run time (`{shown}`), and the uncommitted changes of the repository it targets cannot be checked safely."
                    ),
                    "Run the discard as its own command from the target directory, or retry with bash(command, allow_destructive_git=True).",
                    late,
                ));
            }
            let mut status = String::from("{git} status --porcelain --untracked-files=all");
            if discard == Discard::IncludingIgnored {
                status.push_str(" --ignored=matching");
            }
            let command = call.probe_command(&status, check.script.prefix);
            for dir in dirs {
                let key = format!("{}\0{command}", dir.display());
                if probed.contains(&key) {
                    continue;
                }
                probed.push(key);
                let Ok(Some((listing, truncated))) =
                    probe(check.context, &command, dir, STATUS_LIMITS)
                else {
                    // Not a repository, git missing, a failed or slow probe:
                    // git decides on its own; the guard does not guess.
                    continue;
                };
                let mut listing = listing;
                if truncated {
                    if let Some(cut) = listing.rfind('\n') {
                        listing.truncate(cut);
                    }
                }
                let paths: Vec<String> = listing
                    .split('\n')
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
                    .collect();
                if !paths.is_empty() {
                    return Some(dirty_tree(
                        &paths,
                        discard == Discard::IncludingIgnored,
                        late,
                    ));
                }
            }
        }
        let (node, evidence) = opaque::evidenced(check.model, &opaque::ANY, discard_evidence)?;
        Some(refusal(
            &format!(
                "{}, and the uncommitted changes of the repository it targets cannot be checked safely.",
                opaque::reason(node, &evidence)
            ),
            "Run the discard directly, or retry with bash(command, allow_destructive_git=True).",
            late,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Discard {
    Changes,
    /// `git clean -x`/`-X`: ignored files go too.
    IncludingIgnored,
}

/// Whether `call` discards working-tree changes.
fn discard(call: &GitCall<'_>) -> Option<Discard> {
    let subcommand = call.subcommand?.known()?;
    let args = call.args;
    let options = |args: &[Arg]| -> Vec<String> {
        args.iter()
            .take_while(|arg| arg.known() != Some("--"))
            .filter_map(|arg| {
                arg.known()
                    .filter(|text| text.starts_with('-'))
                    .map(str::to_string)
            })
            .collect()
    };
    let whole_tree = |args: &[Arg]| {
        args.iter().any(|arg| {
            arg.known().is_some_and(|text| {
                matches!(text, "." | "./" | ":/" | ":/." | "*")
                    || text.starts_with("--pathspec-from-file")
            })
        })
    };
    match subcommand {
        "checkout" => {
            let flags = options(args);
            // `-b`/`-B`/`--orphan` create a branch: the next words are its
            // name and start point, never a pathspec.
            let creates = flags.iter().any(|flag| {
                flag.starts_with("--orphan")
                    || (!flag.starts_with("--")
                        && flag.trim_start_matches('-').starts_with(['b', 'B']))
            });
            if creates {
                return None;
            }
            let forced = flags
                .iter()
                .any(|flag| flag == "--force" || (!flag.starts_with("--") && flag.contains('f')));
            (whole_tree(args)
                || (forced
                    && args
                        .iter()
                        .any(|arg| arg.known().is_some_and(|text| !text.starts_with('-')))))
            .then_some(Discard::Changes)
        }
        "restore" => {
            let flags = options(args);
            let mut worktree = false;
            let mut staged = false;
            for flag in &flags {
                if flag.starts_with("--worktree") {
                    worktree = true;
                } else if flag.starts_with("--staged") {
                    staged = true;
                } else if !flag.starts_with("--") {
                    // `-s` takes the rest of its cluster as the source.
                    let letters = flag[1..].split('s').next().unwrap_or_default();
                    worktree |= letters.contains('W');
                    staged |= letters.contains('S');
                }
            }
            ((worktree || !staged) && whole_tree(args)).then_some(Discard::Changes)
        }
        "reset" => args
            .iter()
            .take_while(|arg| arg.known() != Some("--"))
            .any(|arg| arg.known() == Some("--hard"))
            .then_some(Discard::Changes),
        "clean" => {
            let flags = options(args);
            let dry = flags
                .iter()
                .any(|flag| flag == "--dry-run" || (!flag.starts_with("--") && flag.contains('n')));
            if dry {
                return None;
            }
            let ignored = flags
                .iter()
                .any(|flag| !flag.starts_with("--") && (flag.contains('x') || flag.contains('X')));
            Some(if ignored {
                Discard::IncludingIgnored
            } else {
                Discard::Changes
            })
        }
        _ => None,
    }
}

/// `-c core.worktree=...`/`core.bare`: the tree the probe would read is not
/// the one the discard hits.
fn relocates_unreadably(call: &GitCall<'_>) -> bool {
    call.config_values().iter().any(|value| match value {
        Some(text) => {
            let key = text
                .split('=')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            key == "core.worktree" || key == "core.bare"
        }
        None => true,
    })
}

/// An opaque node naming git and a discard verb.
fn discard_evidence(text: &str) -> Option<String> {
    find_word(text, "git")?;
    let verb = if find_word(text, "reset").is_some() && text.contains("--hard") {
        "reset --hard"
    } else {
        ["checkout", "restore", "clean"]
            .into_iter()
            .find(|verb| find_word(text, verb).is_some())?
    };
    Some(format!("`git` and `{verb}`"))
}

fn late_bypass_note(late: bool) -> Option<&'static str> {
    late.then_some(
        "PI_BASH_ALLOW_DESTRUCTIVE_GIT appeared after the kernel started, so the guard ignores it: the \
         variable is read once at launch, by the user who starts the kernel. Use bash(command, \
         allow_destructive_git=True) for an intentional discard, or relaunch the kernel with the \
         variable in the environment.",
    )
}

fn with_note(mut lines: Vec<String>, late: bool) -> String {
    if let Some(note) = late_bypass_note(late) {
        lines.push(String::new());
        lines.push(note.to_string());
    }
    lines.join("\n")
}

fn refusal(reason: &str, advice: &str, late: bool) -> String {
    with_note(
        vec![
            format!("Refusing to run this destructive git command: {reason}"),
            String::new(),
            advice.to_string(),
        ],
        late,
    )
}

fn dirty_tree(paths: &[String], includes_ignored: bool, late: bool) -> String {
    let listed = &paths[..paths.len().min(MAX_DIRTY_PATHS_LISTED)];
    let elided = paths.len() - listed.len();
    let noun = if includes_ignored {
        "uncommitted or ignored file(s)"
    } else {
        "uncommitted change(s)"
    };
    let mut lines = vec![format!(
        "Refusing to run this destructive git command: the working tree has {} {noun}.",
        paths.len()
    )];
    lines.extend(listed.iter().map(|line| format!("  {line}")));
    if elided > 0 {
        lines.push(format!("  ... and {elided} more"));
    }
    lines.push(String::new());
    lines.push("Commit, stash, or stage your work first.".to_string());
    lines.push(
        "To discard these changes intentionally, retry with bash(command, allow_destructive_git=True)."
            .to_string(),
    );
    with_note(lines, late)
}

/// Whether `command` holds a git command that discards uncommitted
/// working-tree changes (the kernel's `is_destructive_git_discard_command`):
/// the model of the text alone, no filesystem and no probe.
pub(crate) fn is_discard_command(command: &str) -> bool {
    let context = GuardContext::new("/nonexistent/pa-bash-discard-check", BTreeMap::new());
    let model = Model::build(&Script::bare(command), &context);
    model
        .invocations
        .iter()
        .filter_map(git_call)
        .any(|call| discard(&call).is_some())
        || opaque::evidenced(&model, &opaque::ANY, discard_evidence).is_some()
}
