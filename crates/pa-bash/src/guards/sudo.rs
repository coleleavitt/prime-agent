//! Privilege-escalation rule: a command that runs `sudo` or `doas` (in any
//! case, by path, behind wrappers, inside nested code, through a `hash -p`
//! entry, or as a command word whose glob or brace spelling can produce
//! `sudo`/`doas`) is refused: root escapes the containment every other
//! guard relies on.

use super::{opaque, Check, Rule};
use crate::model::evidence::first_word;
use crate::model::{Arg, Invocation};
use crate::verdict::GuardKind;

pub(crate) struct Sudo;

const LATE_BYPASS_WARNING: &str = "prime-agent bash: PI_BASH_ALLOW_SUDO appeared after kernel start and is ignored; the sudo guard only honors it when the kernel is started with it set.";
const ESCALATORS: [&str; 2] = ["sudo", "doas"];

impl Rule for Sudo {
    const GUARD: GuardKind = GuardKind::Sudo;
    const LATE_BYPASS_WARNING: Option<&'static str> = Some(LATE_BYPASS_WARNING);

    fn judge(check: &Check<'_>) -> Option<String> {
        for invocation in &check.model.invocations {
            if let Some(violation) = violation(invocation) {
                return Some(message(&violation));
            }
        }
        let (node, evidence) = opaque::evidenced(check.model, &opaque::ANY, |text| {
            first_word(text, &ESCALATORS).map(|word| format!("`{word}`"))
        })?;
        Some(message(&opaque::reason(node, &evidence)))
    }
}

fn message(violation: &str) -> String {
    format!(
        "Refusing to run this command: {violation}. sudo and doas run the command as root (or another user), which escapes the containment every other guard relies on; on a passwordless-sudo setup the escalation is silent. Bypass deliberately, so the intent stays visible in the transcript: call bash(command, allow_sudo=True), or start the kernel with PI_BASH_ALLOW_SUDO=1."
    )
}

fn escalates(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name).to_lowercase();
    ESCALATORS.contains(&base.as_str())
}

fn violation(invocation: &Invocation) -> Option<String> {
    if invocation
        .context
        .iter()
        .any(|via| matches!(via, crate::model::Via::Remote { .. }))
    {
        // Root on another host does not escape this machine's containment.
        return None;
    }
    let detail = || {
        let place = invocation.place();
        if place.is_empty() {
            String::new()
        } else {
            format!(" ({place}: `{}`)", invocation.shown())
        }
    };
    if let Some(layer) = invocation
        .layers
        .iter()
        .find(|layer| escalates(&layer.name))
    {
        return Some(format!(
            "{} would run this command as root or another user{}",
            layer.name,
            detail()
        ));
    }
    match invocation.argv.first()? {
        Arg::Known(text) if escalates(text) => {
            let name = text.rsplit('/').next().unwrap_or(text);
            Some(format!(
                "{name} would run this command as root or another user{}",
                detail()
            ))
        }
        Arg::Pattern(pattern) if ESCALATORS.iter().any(|name| invocation.runs(name)) => {
            let name = pattern.rsplit('/').next().unwrap_or(pattern);
            Some(format!(
                "{name} would run this command as root or another user{}",
                detail()
            ))
        }
        Arg::Known(_) | Arg::Pattern(_) | Arg::Unknown(_) => None,
    }
}
