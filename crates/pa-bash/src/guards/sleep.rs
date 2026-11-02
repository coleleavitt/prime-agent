//! Foreground-sleep rule: a command whose first command is exactly
//! `sleep N` with `N` of 25 seconds or more is refused. A wait that long is a
//! poll: it holds the turn open for something the session can be told about
//! (`rlm.monitor` over an `until` loop, a `bash()` handle's done notice).
//!
//! Only the first command of the script counts, so `until CHECK; do sleep 2;
//! done`, `for ...; do sleep 30; done`, a subshell, `sleep 5` and
//! `sleep 24 && sleep 24` pass, and so does a sleep that runs in the
//! background (`sleep 60 &`).

use super::{Check, Rule};
use crate::syntax::ast::Command;
use crate::syntax::parse::parse;
use crate::verdict::GuardKind;

pub(crate) struct Sleep;

const LATE_BYPASS_WARNING: &str = "prime-agent bash: PI_BASH_ALLOW_SLEEP appeared after kernel start and is ignored; the sleep guard only honors it when the kernel is started with it set.";
const REFUSED_FROM_SECONDS: f64 = 25.0;

impl Rule for Sleep {
    const GUARD: GuardKind = GuardKind::Sleep;
    const LATE_BYPASS_WARNING: Option<&'static str> = Some(LATE_BYPASS_WARNING);

    fn judge(check: &Check<'_>) -> Option<String> {
        let list = parse(check.script.command).list;
        let first = list.items.first()?;
        if first.background || first.first.negated {
            return None;
        }
        let Command::Simple(command) = first.first.commands.first()? else {
            return None;
        };
        let [program, amount] = command.words.as_slice() else {
            return None;
        };
        if program.raw != "sleep" || !command.assignments.is_empty() {
            return None;
        }
        let (whole, fraction) = amount.raw.split_once('.').unwrap_or((&amount.raw, ""));
        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let seconds: f64 = amount.raw.parse().ok()?;
        if seconds < REFUSED_FROM_SECONDS {
            return None;
        }
        let amount = &amount.raw;
        let what =
            if list.items.len() > 1 || !first.rest.is_empty() || first.first.commands.len() > 1 {
                format!("sleep {amount} followed by other commands")
            } else {
                format!("standalone sleep {amount}")
            };
        Some(format!(
            "Refusing to run this command: {what}. A long foreground sleep holds the turn open for a wait you can be notified about. To wait for a condition, run rlm.monitor over an until-loop (e.g. rlm.monitor(\"until <check>; do sleep 2; done\")): you get a notice when the loop exits. To wait for a command you started, keep its bash() handle and end the turn: its done notice arrives on its own. Do not chain shorter sleeps to work around this. Bypass deliberately, so the intent stays visible in the transcript: call bash(command, allow_sleep=True), or start the kernel with PI_BASH_ALLOW_SLEEP=1."
        ))
    }
}
