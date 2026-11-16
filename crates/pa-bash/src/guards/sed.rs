//! In-place sed rule: `sed -i` (any spelling: `-i.bak`, `-Ei`,
//! `--in-place`) on a project source file is refused in favour of the edit
//! skill. A sed write skips the /rewind checkpoint, the permission rules and
//! the stale-read check, and in-place sed edits of source broke the build
//! three to four times as often as edit-tool writes in the 2026-11-15
//! corpus. Range reads (`sed -n 'A,Bp'`), stream edits in a pipe, scratch
//! files and files outside the project pass.

use std::path::Path;

use super::grep::{known_words, runs_in};
use super::{Check, Rule};
use crate::model::Invocation;
use crate::search::{is_scratch, is_source_file, outside_project, sed_in_place};
use crate::verdict::GuardKind;

pub(crate) struct Sed;

const LATE_BYPASS_WARNING: &str = "prime-agent bash: PI_BASH_ALLOW_SED appeared after kernel start and is ignored; the sed guard only honors it when the kernel is started with it set.";

impl Rule for Sed {
    const GUARD: GuardKind = GuardKind::Sed;
    const LATE_BYPASS_WARNING: Option<&'static str> = Some(LATE_BYPASS_WARNING);

    fn judge(check: &Check<'_>) -> Option<String> {
        let project = check.context.cwd();
        check.model.invocations.iter().find_map(|invocation| {
            let file = edited_source(invocation, project)?;
            Some(message(invocation, &file))
        })
    }
}

/// The first project source file `invocation` edits in place with sed.
fn edited_source(invocation: &Invocation, project: &Path) -> Option<String> {
    let (program, args) = known_words(invocation)?;
    if !matches!(program.as_str(), "sed" | "gsed") || !runs_in(invocation, project) {
        return None;
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    sed_in_place(&args)?.files.into_iter().find(|file| {
        is_source_file(file)
            && !is_scratch(file)
            && !outside_project(file)
            && (!Path::new(file).is_absolute() || Path::new(file).starts_with(project))
    })
}

fn message(invocation: &Invocation, file: &str) -> String {
    format!(
        "Refusing to run this command: `{}` edits the source file {file} in place with sed. A sed write skips the /rewind checkpoint, the permission rules and the stale-read check, and in-place sed edits of source broke the build three to four times as often as edit-skill writes. Use the edit skill: await edit.run(path, old_str, new_str) for a replacement (replace_all=True for every occurrence), or await edit.write(path, text) for a whole file. If sed is really what you need (a mechanical rewrite across a file), bypass deliberately: bash(command, allow_sed=True), or start the kernel with PI_BASH_ALLOW_SED=1.",
        invocation.shown()
    )
}
