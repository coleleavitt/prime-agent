//! Pipe-to-shell rule: a download (`curl`, `wget`, `fetch`) whose output a
//! shell runs is refused: piped into a shell (`curl URL | sh`, through `cat`
//! or `sudo bash`), substituted into a shell's code (`sh -c "$(curl URL)"`,
//! `bash <(curl URL)`, `eval "$(wget -qO- URL)"`), or piped into a command
//! word only known at run time (`curl URL | $SHELL`).

use super::{Check, Rule, opaque};
use crate::model::evidence::first_word;
use crate::model::{Arg, Input, Invocation, Model, OpaqueKind};
use crate::verdict::GuardKind;

pub(crate) struct PipeToShell;

const LATE_BYPASS_WARNING: &str = "prime-agent bash: PI_BASH_ALLOW_PIPE_TO_SHELL appeared after kernel start and is ignored; the pipe-to-shell guard only honors it when the kernel is started with it set.";
const DOWNLOADERS: [&str; 3] = ["curl", "wget", "fetch"];

impl Rule for PipeToShell {
    const GUARD: GuardKind = GuardKind::PipeToShell;
    const LATE_BYPASS_WARNING: Option<&'static str> = Some(LATE_BYPASS_WARNING);

    fn judge(check: &Check<'_>) -> Option<String> {
        let model = check.model;
        for invocation in &model.invocations {
            if let Some(phrase) = violation(model, invocation) {
                return Some(message(&phrase));
            }
        }
        // `fetch` is too common a word (`git fetch`) to be evidence.
        let download =
            |text: &str| first_word(text, &["curl", "wget"]).map(|word| format!("`{word}`"));
        // A shell reading a pipeline is judged on the pipeline's own commands
        // above; a stream from elsewhere or code built at run time needs its
        // visible source to show a download.
        let (node, evidence) =
            opaque::evidenced(model, &[OpaqueKind::Stdin, OpaqueKind::Dynamic], download).or_else(
                || {
                    // Unreadable text: a download and a runner must both show.
                    opaque::evidenced(model, &[OpaqueKind::Unparsed], |text| {
                        let runner = first_word(
                            text,
                            &["sh", "bash", "zsh", "dash", "ksh", "eval", "source"],
                        )?;
                        download(text).map(|download| format!("{download} and `{runner}`"))
                    })
                },
            )?;
        let kind = match node.kind {
            OpaqueKind::Stdin => "a download piped into a shell",
            OpaqueKind::Dynamic => "a download substituted into a shell",
            OpaqueKind::Script
            | OpaqueKind::Pipe
            | OpaqueKind::CommandWord
            | OpaqueKind::Unparsed => "a download whose output reaches code the guard cannot read",
        };
        Some(message(&format!(
            "{kind}: {}",
            opaque::reason(node, &evidence)
        )))
    }
}

fn is_download(invocation: &Invocation) -> bool {
    invocation
        .program()
        .is_some_and(|name| DOWNLOADERS.contains(&name))
}

fn is_runner(invocation: &Invocation) -> bool {
    invocation.reads_code_from_stdin
        || invocation
            .program()
            .is_some_and(crate::model::wrappers::is_code_runner)
}

fn violation(model: &Model, invocation: &Invocation) -> Option<String> {
    let shown = || {
        let place = invocation.place();
        if place.is_empty() {
            format!("`{}`", invocation.shown())
        } else {
            format!("`{}` {place}", invocation.shown())
        }
    };
    let piped = invocation.stdin == Input::Pipe;
    if piped
        && (invocation.reads_code_from_stdin
            || matches!(invocation.argv.first(), Some(Arg::Unknown(_))))
    {
        if let Some(download) = model
            .producers(invocation)
            .into_iter()
            .find(|producer| is_download(producer))
        {
            let into = if invocation.reads_code_from_stdin {
                "a shell"
            } else {
                "a command the scan cannot resolve"
            };
            return Some(format!(
                "a download piped into {into}: `{}` feeds {}",
                download.shown(),
                shown()
            ));
        }
    }
    // `$(curl URL) args`: the download becomes the command line.
    if matches!(invocation.argv.first(), Some(Arg::Unknown(_))) {
        for capture in model
            .captures
            .iter()
            .filter(|capture| capture.consumer == invocation.index)
        {
            if let Some(download) = model.invocations[capture.range.clone()]
                .iter()
                .find(|inner| is_download(inner))
            {
                return Some(format!(
                    "a download substituted into a shell command: `{}` becomes the command line {} runs",
                    download.shown(),
                    shown()
                ));
            }
        }
    }
    if is_runner(invocation) {
        for range in &invocation.code_substitutions {
            if let Some(download) = model.invocations[range.clone()]
                .iter()
                .find(|inner| is_download(inner))
            {
                return Some(format!(
                    "a download substituted into a shell: `{}` becomes the code {} runs",
                    download.shown(),
                    shown()
                ));
            }
        }
    }
    None
}

fn message(violation: &str) -> String {
    [
        "Refusing to run this command: piping or substituting curl/wget".to_string(),
        "output into a shell interpreter downloads and executes remote code".to_string(),
        format!("without review ({violation})."),
        String::new(),
        "Download the script to a file, read the file, then run it in a".to_string(),
        "later command (curl -o script.sh URL, then sh script.sh).".to_string(),
        String::new(),
        "If the download is trusted, retry with".to_string(),
        "bash(command, allow_pipe_to_shell=True), or start the kernel with".to_string(),
        "PI_BASH_ALLOW_PIPE_TO_SHELL=1; the variable is frozen at kernel start,".to_string(),
        "so writing it mid-session never unlocks the guard.".to_string(),
    ]
    .join("\n")
}
