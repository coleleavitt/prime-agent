//! Self-match rule: a process-matching kill whose pattern matches the
//! command's own shell kills that shell (and the command with it) before it
//! finishes: `pkill -f PATTERN` where PATTERN occurs in the command's text
//! (the shell runs as `SHELL -c TEXT`), `kill $(pgrep -f PATTERN)` and
//! `pgrep -f PATTERN | xargs kill` likewise, and `pkill NAME` /
//! `killall [-r] NAME` matching the shell's own process name.
//!
//! The bracket trick (`pkill -f '[b]urp'`) matches `burp` but not its own
//! spelling, so it passes.

use super::{Check, Rule};
use crate::model::{Arg, Invocation, Model, Output};
use crate::shell::resolve_shell;
use crate::verdict::GuardKind;

pub(crate) struct SelfMatch;

const LATE_BYPASS_WARNING: &str = "prime-agent bash: PI_BASH_ALLOW_SELF_MATCH appeared after kernel start and is ignored; the self-match guard only honors it when the kernel is started with it set.";

impl Rule for SelfMatch {
    const GUARD: GuardKind = GuardKind::SelfMatch;
    const LATE_BYPASS_WARNING: Option<&'static str> = Some(LATE_BYPASS_WARNING);

    fn judge(check: &Check<'_>) -> Option<String> {
        let shell = resolve_shell(check.context)
            .ok()
            .map_or_else(|| "bash".to_string(), |path| path.display().to_string());
        let own = Own {
            cmdline: format!("{shell} -c {}", check.script.script),
            names: shell_names(check.model, &shell),
        };
        let kills = check.model.invocations.iter().any(is_kill);
        for invocation in &check.model.invocations {
            let Some(program) = invocation.program() else {
                continue;
            };
            let matched = match program {
                "pkill" => match_pattern(invocation, &own),
                "pgrep" if kills && feeds_a_kill(check.model, invocation) => {
                    match_pattern(invocation, &own)
                }
                "killall" => match_killall(invocation, &own),
                _ => None,
            };
            if let Some(found) = matched {
                return Some(message(invocation, &found));
            }
        }
        None
    }
}

/// What the command's own shell looks like to a process matcher.
struct Own {
    /// `/proc/PID/cmdline` of the shell, joined by spaces.
    cmdline: String,
    /// The process names (`comm`) of the shells the command runs.
    names: Vec<String>,
}

fn shell_names(model: &Model, shell: &str) -> Vec<String> {
    let mut names = vec![comm(shell)];
    for invocation in &model.invocations {
        if let Some(program) = invocation.program() {
            if crate::model::wrappers::is_shell(program) && !names.contains(&comm(program)) {
                names.push(comm(program));
            }
        }
    }
    names
}

/// A process name as the kernel keeps it: the basename, cut at 15 bytes.
fn comm(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.chars().take(15).collect()
}

fn is_kill(invocation: &Invocation) -> bool {
    invocation.program() == Some("kill")
}

/// `kill $(pgrep ...)`, `pgrep ... | xargs kill`.
fn feeds_a_kill(model: &Model, invocation: &Invocation) -> bool {
    match &invocation.stdout {
        Output::Captured => true,
        Output::Pipe => model.consumers(invocation).into_iter().any(is_kill),
        Output::Transcript | Output::File(_) => false,
    }
}

/// What matched: the pattern and the text it matched.
struct Found {
    pattern: String,
    target: String,
    full: bool,
}

/// pgrep/pkill options that take a value.
const PGREP_VALUES: &str = "dgGPstuUFqr";

fn match_pattern(invocation: &Invocation, own: &Own) -> Option<Found> {
    let mut full = false;
    let mut exact = false;
    let mut ignore_case = false;
    let mut inverse = false;
    let mut pattern: Option<&str> = None;
    let mut args = invocation.argv[1..].iter();
    while let Some(arg) = args.next() {
        let Some(text) = arg.known() else {
            // A pattern only known at run time: no evidence of a self-match.
            return None;
        };
        if text == "--" {
            pattern = args.next().and_then(Arg::known);
            break;
        }
        if let Some(long) = text.strip_prefix("--") {
            match long {
                "full" => full = true,
                "exact" => exact = true,
                "ignore-case" => ignore_case = true,
                "inverse" => inverse = true,
                "signal" | "session" | "terminal" | "euid" | "uid" | "group" | "pgroup"
                | "parent" | "pidfile" | "ns" | "nslist" | "runstates" | "queue" | "delimiter" => {
                    args.next();
                }
                _ => {}
            }
            continue;
        }
        if let Some(flags) = text.strip_prefix('-') {
            if flags.is_empty() {
                continue;
            }
            // `-9`, `-KILL`, `-SIGTERM`: a signal (pkill's first argument).
            if flags.chars().all(|ch| ch.is_ascii_digit())
                || (flags
                    .chars()
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
                    && flags.len() > 1)
            {
                continue;
            }
            for (at, letter) in flags.chars().enumerate() {
                match letter {
                    'f' => full = true,
                    'x' => exact = true,
                    'i' => ignore_case = true,
                    'v' => inverse = true,
                    letter if PGREP_VALUES.contains(letter) => {
                        if at + 1 == flags.len() {
                            args.next();
                        }
                        break;
                    }
                    _ => {}
                }
            }
            continue;
        }
        pattern = Some(text);
        break;
    }
    let pattern = pattern?;
    let regex = compile(pattern, exact, ignore_case)?;
    if full {
        let matched = regex
            .find(&own.cmdline)
            .map(|found| found.as_str().to_string());
        return match (matched, inverse) {
            (Some(target), false) => Some(Found {
                pattern: pattern.to_string(),
                target,
                full,
            }),
            (None, true) => Some(Found {
                pattern: pattern.to_string(),
                target: "every process it does not match, its own shell included".to_string(),
                full,
            }),
            (Some(_), true) | (None, false) => None,
        };
    }
    own.names
        .iter()
        .find(|name| regex.is_match(name) != inverse)
        .map(|name| Found {
            pattern: pattern.to_string(),
            target: name.clone(),
            full,
        })
}

fn match_killall(invocation: &Invocation, own: &Own) -> Option<Found> {
    let mut regex_mode = false;
    let mut ignore_case = false;
    let mut names = Vec::new();
    let mut args = invocation.argv[1..].iter();
    while let Some(arg) = args.next() {
        let text = arg.known()?;
        if let Some(long) = text.strip_prefix("--") {
            match long {
                "regexp" => regex_mode = true,
                "ignore-case" => ignore_case = true,
                "signal" | "user" | "older-than" | "younger-than" | "context" | "ns" => {
                    args.next();
                }
                _ => {}
            }
            continue;
        }
        if let Some(flags) = text.strip_prefix('-') {
            if flags.chars().all(|ch| ch.is_ascii_digit())
                || flags.chars().all(|ch| ch.is_ascii_uppercase())
            {
                continue;
            }
            for (at, letter) in flags.chars().enumerate() {
                match letter {
                    'r' => regex_mode = true,
                    'I' => ignore_case = true,
                    's' | 'u' | 'o' | 'y' | 'Z' | 'n' => {
                        if at + 1 == flags.len() {
                            args.next();
                        }
                        break;
                    }
                    _ => {}
                }
            }
            continue;
        }
        names.push(text);
    }
    for name in names {
        let hit = if regex_mode {
            compile(name, false, ignore_case)
                .and_then(|regex| own.names.iter().find(|own| regex.is_match(own)).cloned())
        } else {
            own.names
                .iter()
                .find(|own| {
                    if ignore_case {
                        own.eq_ignore_ascii_case(name)
                    } else {
                        *own == name
                    }
                })
                .cloned()
        };
        if let Some(target) = hit {
            return Some(Found {
                pattern: name.to_string(),
                target,
                full: false,
            });
        }
    }
    None
}

/// A POSIX extended regular expression as the matcher reads it; an
/// expression the engine cannot read is matched as literal text.
fn compile(pattern: &str, exact: bool, ignore_case: bool) -> Option<regex::Regex> {
    let build = |source: &str| {
        regex::RegexBuilder::new(&if exact {
            format!("^(?:{source})$")
        } else {
            source.to_string()
        })
        .case_insensitive(ignore_case)
        .size_limit(1 << 20)
        .build()
        .ok()
    };
    build(pattern).or_else(|| build(&regex::escape(pattern)))
}

fn message(invocation: &Invocation, found: &Found) -> String {
    let what = if found.full {
        format!(
            "its pattern \"{}\" matches this command's own text (\"{}\"), and the shell running the command has that text on its command line",
            found.pattern,
            crate::model::shorten(&found.target, 80)
        )
    } else {
        format!(
            "its pattern \"{}\" matches the process name of the shell running this command ({})",
            found.pattern, found.target
        )
    };
    let place = invocation.place();
    let place = if place.is_empty() {
        String::new()
    } else {
        format!(" ({place})")
    };
    format!(
        "Refusing to run this command: `{}`{place} would kill its own shell: {what}, so the command dies (SIGKILL or SIGTERM) before it finishes.\n\nMatch the target precisely instead: pkill -x NAME for an exact process name, a pid (kill PID), or the bracket trick (pkill -f '[{}]{}' matches the process but not this command's text).\n\nTo run it as written, retry with bash(command, allow_self_match=True), or start the kernel with PI_BASH_ALLOW_SELF_MATCH=1.",
        invocation.shown(),
        found.pattern.chars().next().unwrap_or('x'),
        found.pattern.chars().skip(1).collect::<String>(),
    )
}
