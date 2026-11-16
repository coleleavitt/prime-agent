//! Structural-grep rule: a grep, ripgrep, ag or `git grep` whose every
//! pattern is a definition (`fn name`, `class Name`) or a call site
//! (`name(`), over project source, printing lines, is refused in favour of
//! the structural tools (`rlm.lsp`, `codeflow`). Pipe filters, counts and
//! file lists, plain-text searches, logs and data, and code outside the
//! project pass (`crate::search` has the shapes and their corpus).

use std::path::Path;

use super::{Check, Rule};
use crate::model::{Arg, Input, Invocation};
use crate::search::{SearchShape, StructuralSearch, structural_grep};
use crate::verdict::GuardKind;

pub(crate) struct Grep;

const LATE_BYPASS_WARNING: &str = "prime-agent bash: PI_BASH_ALLOW_GREP appeared after kernel start and is ignored; the grep guard only honors it when the kernel is started with it set.";

impl Rule for Grep {
    const GUARD: GuardKind = GuardKind::Grep;
    const LATE_BYPASS_WARNING: Option<&'static str> = Some(LATE_BYPASS_WARNING);

    fn judge(check: &Check<'_>) -> Option<String> {
        let project = check.context.cwd();
        check.model.invocations.iter().find_map(|invocation| {
            let found = structural(invocation, project)?;
            Some(message(invocation, &found))
        })
    }
}

/// The program and argument words of `invocation` when all are fixed text
/// (`git grep` reads as the program `git-grep`).
pub(crate) fn known_words(invocation: &Invocation) -> Option<(String, Vec<String>)> {
    let mut words = Vec::with_capacity(invocation.argv.len());
    for arg in &invocation.argv {
        match arg {
            Arg::Known(text) | Arg::Pattern(text) => words.push(text.clone()),
            Arg::Unknown(_) => return None,
        }
    }
    let program = invocation.program()?.to_string();
    if program == "git" {
        let subcommand = words
            .iter()
            .skip(1)
            .position(|word| !word.starts_with('-'))?
            + 1;
        if words[subcommand] != "grep" {
            return None;
        }
        return Some(("git-grep".to_string(), words[subcommand + 1..].to_vec()));
    }
    Some((program, words.into_iter().skip(1).collect()))
}

/// Whether `invocation` runs inside `project` (every directory it may run
/// in lies under it).
pub(crate) fn runs_in(invocation: &Invocation, project: &Path) -> bool {
    invocation.cwd.unknown.is_none()
        && invocation
            .cwd
            .dirs
            .iter()
            .all(|dir| dir.starts_with(project))
}

fn structural(invocation: &Invocation, project: &Path) -> Option<StructuralSearch> {
    let (program, args) = known_words(invocation)?;
    if !runs_in(invocation, project) {
        return None;
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let found = structural_grep(&program, &args, matches!(invocation.stdin, Input::Pipe))?;
    let outside = found
        .args
        .paths
        .iter()
        .any(|path| Path::new(path).is_absolute() && !Path::new(path).starts_with(project));
    (!outside).then_some(found)
}

fn message(invocation: &Invocation, found: &StructuralSearch) -> String {
    let what = match found.shape {
        SearchShape::Definition => format!("looks up the definition `{}`", found.pattern),
        SearchShape::CallSite => format!("looks up the call sites `{}`", found.pattern),
    };
    format!(
        "Refusing to run this command: `{}` {what} in project source with grep. Use the structural tools and quote their file:line output: definitions with rlm.lsp.definition(path, line, column) or rlm.lsp.workspace_symbols(\"name\"), callers with codeflow.tree(path, name, direction=\"incoming\") or rlm.lsp.references(path, line, column), what a function calls with codeflow.tree(path, name), and every path through it with codeflow.cfg(path, name). grep only finds candidates, and the user has asked for the structural tools repeatedly. If a plain text search is really what you need, bypass deliberately: bash(command, allow_grep=True), or start the kernel with PI_BASH_ALLOW_GREP=1.",
        invocation.shown()
    )
}
