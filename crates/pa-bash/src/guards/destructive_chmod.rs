//! Recursive chmod/chown rule: `chmod -R`, `chown -R`, `chgrp -R` (or an
//! option only known at run time that can be `-R`) is refused while a target
//! resolves outside the kernel workspace, onto the home directory, the
//! filesystem root, or a dot-directory or dotfile inside the workspace.
//! Targets are resolved the way the shell and the command will: relative to
//! the directory the command runs in (after the `cd`s before it), with
//! globs expanded against the filesystem and symlinks followed.

use std::path::{Component, Path, PathBuf};

use super::{opaque, Check, Rule};
use crate::model::evidence::first_word;
use crate::model::files::{expand, join, unescape};
use crate::model::{Arg, Invocation, Via};
use crate::verdict::GuardKind;

pub(crate) struct DestructiveChmod;

const LATE_BYPASS_WARNING: &str = "prime-agent bash: PI_BASH_ALLOW_DESTRUCTIVE_CHMOD appeared after kernel start and is ignored; the recursive chmod/chown guard only honors it when the kernel is started with it set.";
const BYPASS_ENV: &str = "PI_BASH_ALLOW_DESTRUCTIVE_CHMOD";
const COMMANDS: [&str; 3] = ["chmod", "chown", "chgrp"];

impl Rule for DestructiveChmod {
    const GUARD: GuardKind = GuardKind::DestructiveChmod;
    const LATE_BYPASS_WARNING: Option<&'static str> = Some(LATE_BYPASS_WARNING);

    fn judge(check: &Check<'_>) -> Option<String> {
        let places = Places::new(check);
        for invocation in &check.model.invocations {
            if let Some(message) = judge_invocation(invocation, &places) {
                return Some(message);
            }
        }
        let (node, evidence) =
            opaque::evidenced(check.model, &opaque::ANY, recursive_chmod_evidence)?;
        Some(format!(
            "Refusing to run this recursive chmod/chown command: {}, so the directories it targets cannot be resolved safely.{}",
            opaque::reason(node, &evidence),
            closing("Run the chmod/chown directly, or retry with")
        ))
    }
}

fn closing(advice: &str) -> String {
    format!("\n\n{advice} bash(command, allow_destructive_chmod=True), or start the kernel with {BYPASS_ENV}=1.")
}

struct Places {
    workspace: PathBuf,
    home: Option<PathBuf>,
}

impl Places {
    fn new(check: &Check<'_>) -> Self {
        let cwd = check.context.cwd();
        Self {
            workspace: cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()),
            home: check
                .context
                .var("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| resolve(Path::new(home))),
        }
    }
}

fn judge_invocation(invocation: &Invocation, places: &Places) -> Option<String> {
    if !COMMANDS.iter().any(|name| invocation.runs(name)) {
        return None;
    }
    let args = &invocation.argv[1..];
    let mut recursive = false;
    let mut maybe_recursive = false;
    let mut reference = false;
    let mut positionals: Vec<&Arg> = Vec::new();
    let mut options_done = false;
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        match arg {
            Arg::Known(text) if !options_done && text == "--" => options_done = true,
            Arg::Known(text) if !options_done && text.starts_with("--") => {
                let name = text.split('=').next().unwrap_or(text);
                if name.len() >= 5 && "--recursive".starts_with(name) {
                    recursive = true;
                } else if name.starts_with("--ref") && "--reference".starts_with(name) {
                    reference = true;
                    skip = !text.contains('=');
                } else if name == "--from" {
                    skip = !text.contains('=');
                }
            }
            Arg::Known(text) if !options_done && text.starts_with('-') && text.len() > 1 => {
                if text.contains('R') {
                    recursive = true;
                }
            }
            // A value only known at run time in option position (before
            // the mode) can be `-R`; a path xargs or find supplies is data.
            Arg::Unknown(unknown)
                if !options_done
                    && positionals.is_empty()
                    && !unknown.input
                    && arg.may_start_with('-') =>
            {
                maybe_recursive = true;
                positionals.push(arg);
            }
            Arg::Known(_) | Arg::Pattern(_) | Arg::Unknown(_) => positionals.push(arg),
        }
    }
    if !recursive && !maybe_recursive {
        return None;
    }
    let targets: &[&Arg] = if reference {
        &positionals
    } else {
        positionals.get(1..).unwrap_or(&[])
    };
    let remote = invocation.context.iter().find_map(|via| match via {
        Via::Remote { host } => Some(host.clone()),
        _ => None,
    });
    for target in targets {
        let violation = match &remote {
            Some(host) => {
                remote_violation(target).map(|reason| (reason, None, Some(host.as_str())))
            }
            None => local_violation(invocation, target, places)
                .map(|(reason, resolved)| (reason, resolved, None)),
        };
        if let Some((reason, resolved, host)) = violation {
            return Some(operand_message(
                invocation,
                target,
                &reason,
                resolved.as_deref(),
                host,
                places,
            ));
        }
    }
    None
}

enum Reason {
    Root,
    Home,
    Escapes,
    Dot,
    TopLevel,
    Unresolvable(String),
    Relocated(String),
}

impl Reason {
    fn phrase(&self) -> String {
        match self {
            Reason::Root => "names the filesystem root (/)".to_string(),
            Reason::Home => "names the home directory".to_string(),
            Reason::Escapes => "escapes the kernel workspace".to_string(),
            Reason::Dot => "names a dot-directory or dotfile (e.g. .git)".to_string(),
            Reason::TopLevel => "names a top-level system directory".to_string(),
            Reason::Unresolvable(why) => format!("cannot be resolved statically ({why})"),
            Reason::Relocated(how) => {
                format!("is relative to a directory only known at run time (after `{how}`)")
            }
        }
    }
}

fn operand_message(
    invocation: &Invocation,
    target: &Arg,
    reason: &Reason,
    resolved: Option<&Path>,
    host: Option<&str>,
    places: &Places,
) -> String {
    let shown = target.shown();
    let resolved = resolved
        .filter(|resolved| resolved.to_string_lossy() != shown)
        .map(|resolved| format!(" ({})", resolved.display()))
        .unwrap_or_default();
    let place = invocation.place();
    let place = if place.is_empty() {
        String::new()
    } else {
        format!(" {place}: `{}`", invocation.shown())
    };
    let policy = match host {
        Some(host) => format!(
            "On a remote host ({host}) a recursive chmod/chown must name a specific directory, never the root, a top-level system directory, or a home directory."
        ),
        None => format!(
            "Recursive chmod/chown must stay inside the kernel workspace ({}) and must never target the home directory, dot-directories (e.g. .git), dotfiles, or the filesystem root.",
            places.workspace.display()
        ),
    };
    format!(
        "Refusing to run this recursive chmod/chown command:\n  the operand \"{shown}\" {}{resolved}{place}.\n{policy}{}",
        reason.phrase(),
        closing("To run it intentionally, retry with")
    )
}

/// Why `target` is out of bounds for a local recursive change, and the path
/// it resolves to.
fn local_violation(
    invocation: &Invocation,
    target: &Arg,
    places: &Places,
) -> Option<(Reason, Option<PathBuf>)> {
    let text = match target {
        Arg::Known(text) => text.clone(),
        Arg::Pattern(pattern) => pattern.clone(),
        Arg::Unknown(unknown) => {
            let why = if unknown.input {
                "its targets come from xargs or find at run time".to_string()
            } else {
                "it is only known at run time".to_string()
            };
            return Some((Reason::Unresolvable(why), None));
        }
    };
    if !text.starts_with('/') {
        if let Some(unknown) = &invocation.cwd.unknown {
            return Some((Reason::Relocated(unknown.clone()), None));
        }
    }
    for dir in &invocation.cwd.dirs {
        let paths = match target {
            Arg::Pattern(pattern) => match expand(dir, pattern) {
                Some(paths) if !paths.is_empty() => paths,
                Some(_) => vec![join(dir, &unescape(pattern))],
                None => {
                    return Some((
                        Reason::Unresolvable("a glob with too many matches".to_string()),
                        None,
                    ))
                }
            },
            Arg::Known(_) | Arg::Unknown(_) => vec![join(dir, &text)],
        };
        for path in paths {
            let resolved = resolve(&path);
            if let Some(reason) = policy(&resolved, places) {
                return Some((reason, Some(resolved)));
            }
        }
    }
    None
}

fn policy(resolved: &Path, places: &Places) -> Option<Reason> {
    if resolved == Path::new("/") {
        return Some(Reason::Root);
    }
    if places.home.as_deref() == Some(resolved) {
        return Some(Reason::Home);
    }
    let Ok(below) = resolved.strip_prefix(&places.workspace) else {
        return Some(Reason::Escapes);
    };
    below
        .components()
        .any(|component| component.as_os_str().to_string_lossy().starts_with('.'))
        .then_some(Reason::Dot)
}

/// A remote target: only the root, top-level directories and home
/// directories are refused (the workspace is local).
fn remote_violation(target: &Arg) -> Option<Reason> {
    let text = match target {
        Arg::Known(text) => text.as_str(),
        Arg::Pattern(_) | Arg::Unknown(_) => {
            let prefix = target.fixed_prefix();
            if prefix.trim_end_matches('/').matches('/').count() >= 2 {
                return None;
            }
            return Some(Reason::Unresolvable(
                "it is only known at run time".to_string(),
            ));
        }
    };
    let normal = join(Path::new("/remote-home"), text);
    let depth = normal
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count();
    if text == "~" || text.starts_with("~/") && depth <= 2 || normal == Path::new("/remote-home") {
        return Some(Reason::Home);
    }
    if normal == Path::new("/") {
        return Some(Reason::Root);
    }
    let home_like =
        normal.starts_with("/home") || normal.starts_with("/root") || normal.starts_with("/Users");
    if depth <= 1 {
        return Some(if home_like {
            Reason::Home
        } else {
            Reason::TopLevel
        });
    }
    (home_like && depth == 2).then_some(Reason::Home)
}

/// `path` with every existing symlink resolved (missing tail components
/// kept as written), like `os.path.realpath`.
fn resolve(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            let mut out = real;
            for component in tail.iter().rev() {
                out.push(component);
            }
            return out;
        }
        match (
            existing.file_name().map(std::ffi::OsStr::to_os_string),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// An opaque node naming chmod/chown/chgrp and a recursive flag.
fn recursive_chmod_evidence(text: &str) -> Option<String> {
    let command = first_word(text, &COMMANDS)?;
    let recursive = text
        .split(|ch: char| ch.is_whitespace() || "'\"`;|&()".contains(ch))
        .find(|token| {
            (token.starts_with('-') && !token.starts_with("--") && token.contains('R'))
                || (token.len() >= 5 && "--recursive".starts_with(token))
        })?;
    Some(format!("`{command}` and `{recursive}`"))
}
