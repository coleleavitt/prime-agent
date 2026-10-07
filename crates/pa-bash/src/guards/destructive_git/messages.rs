//! The guard's refusal messages, byte for byte the kernel's.

/// How many dirty paths the refusal lists before eliding the rest.
const MAX_DIRTY_PATHS_LISTED: usize = 10;

/// The note appended when the bypass variable appeared after kernel start.
fn late_bypass_note(late_bypass: bool) -> Option<&'static str> {
    late_bypass.then_some(
        "PI_BASH_ALLOW_DESTRUCTIVE_GIT appeared after the kernel started, so the guard ignores it: the \
         variable is read once at launch, by the user who starts the kernel. Use bash(command, \
         allow_destructive_git=True) for an intentional discard, or relaunch the kernel with the \
         variable in the environment.",
    )
}

fn with_note(mut lines: Vec<String>, late_bypass: bool) -> String {
    if let Some(note) = late_bypass_note(late_bypass) {
        lines.push(String::new());
        lines.push(note.to_string());
    }
    lines.join("\n")
}

/// Why a discard was refused before (or without) a probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// An `eval` payload hides the discard.
    Eval,
    /// The command word is an expanded value the probe cannot replay.
    RevealedCommand,
    /// The command changes directory or repository first.
    Relocation,
}

pub(super) fn refusal(kind: Refusal, late_bypass: bool) -> String {
    let (reason, advice) = match kind {
        Refusal::Eval => (
            "Refusing to run this destructive git command: it wraps a git discard in eval, and the \
             uncommitted changes of the repository it targets cannot be checked safely.",
            "Run the discard directly, or retry with bash(command, allow_destructive_git=True).",
        ),
        Refusal::RevealedCommand => (
            "Refusing to run this destructive git command: the command word is an expanded value whose \
             argv cannot be replayed, and the uncommitted changes of the repository it targets cannot be \
             checked safely.",
            "Run the discard directly, or retry with bash(command, allow_destructive_git=True).",
        ),
        Refusal::Relocation => (
            "Refusing to run this destructive git command: it changes directory (or repository) first, and \
             the uncommitted changes of the repository it targets cannot be checked safely.",
            "Run the discard as its own command from the target directory, or retry with bash(command, \
             allow_destructive_git=True).",
        ),
    };
    with_note(
        vec![reason.to_string(), String::new(), advice.to_string()],
        late_bypass,
    )
}

/// The dirty-tree refusal listing the probe's paths.
pub(super) fn dirty_tree(
    dirty_paths: &[String],
    includes_ignored_files: bool,
    late_bypass: bool,
) -> String {
    let listed = &dirty_paths[..dirty_paths.len().min(MAX_DIRTY_PATHS_LISTED)];
    let elided = dirty_paths.len() - listed.len();
    let noun = if includes_ignored_files {
        "uncommitted or ignored file(s)"
    } else {
        "uncommitted change(s)"
    };
    let mut lines = vec![format!(
        "Refusing to run this destructive git command: the working tree has {} {noun}.",
        dirty_paths.len()
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
    with_note(lines, late_bypass)
}
