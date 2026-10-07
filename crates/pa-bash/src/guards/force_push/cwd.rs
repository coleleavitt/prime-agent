//! Where a push runs: the statically-known `cd` relocations before it are
//! replayed (logical, like the shell's default `cd -L`), and anything that
//! could relocate but cannot be replayed makes the directory unresolvable.

use crate::context::GuardContext;

use super::budget::{Budget, Scan};
use super::config::Assignments;
use super::lexing::unquoted_paren_counts;
use super::text::{chars, has_git_assignment, has_word, is_space, join_path, slice};
use super::words::{literal_words, scan_words};

/// The directory a push would run in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PushCwd {
    /// No cd moved the shell: the kernel workspace.
    Workspace,
    /// The replayed directory.
    Moved(String),
    /// Something relocates the shell in a way the guard cannot replay.
    Unresolvable,
}

/// One cd/pushd argument read literally, and whether the shell would expand a
/// leading `~` (only when the word as written begins with an unquoted one).
struct StaticArg {
    value: String,
    tilde_expands: bool,
}

/// A bare `cd`: no argument, so it goes to HOME.
const BARE_CD: StaticArg = StaticArg {
    value: String::new(),
    tilde_expands: false,
};

fn static_arg(raw: &str) -> Option<StaticArg> {
    if raw.is_empty() || raw.contains(|ch| "$`;&|()<>#".contains(ch)) {
        return None;
    }
    let (words, well_formed) = literal_words(raw);
    match words.as_slice() {
        [Some(value)] if well_formed && !value.is_empty() => Some(StaticArg {
            value: value.clone(),
            tilde_expands: raw.starts_with('~'),
        }),
        _ => None,
    }
}

/// The home directory the child's `cd` would use: the one the command
/// assigns (`None` when unreadable), else the kernel's (`os.path.expanduser`).
fn home_directory(env: &Assignments, context: &GuardContext) -> Option<String> {
    if let Some(assigned) = env.get("HOME") {
        return assigned.clone();
    }
    let home = match context.var("HOME") {
        Some(home) => home.to_string(),
        None => std::env::home_dir()?.display().to_string(),
    };
    let trimmed = home.trim_end_matches('/');
    Some(if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    })
}

/// True when `CDPATH` could redirect a relative `cd` target (the command's
/// own assignment decides when it makes one).
fn cdpath_redirects(env: &Assignments, context: &GuardContext) -> bool {
    match env.get("CDPATH") {
        Some(assigned) => assigned.as_ref().is_none_or(|value| !value.is_empty()),
        None => context.var("CDPATH").is_some_and(|value| !value.is_empty()),
    }
}

/// Resolve one static `cd` argument against the running directory; `None`
/// when it cannot be resolved (bare `cd` without a usable HOME, `cd -` and
/// options, another user's home, or anything CDPATH could redirect).
fn resolve_cd_target(
    arg: &StaticArg,
    current: Option<&str>,
    workspace: &str,
    env: &Assignments,
    context: &GuardContext,
) -> Option<String> {
    let base = current
        .filter(|current| !current.is_empty())
        .unwrap_or(workspace);
    if arg.value.is_empty() {
        return home_directory(env, context);
    }
    let target = arg.value.as_str();
    if target.starts_with('-') {
        return None;
    }
    if let Some(after) = target.strip_prefix('~') {
        if !arg.tilde_expands {
            return Some(join_path(base, target)); // `cd "~"` enters `./~`
        }
        if after.is_empty() || after.starts_with('/') {
            let home = home_directory(env, context)?;
            return Some(match after.strip_prefix('/') {
                Some(rest) => join_path(&home, rest),
                None => home,
            });
        }
        return None; // ~otheruser
    }
    if target.starts_with('/') {
        return Some(target.to_string());
    }
    if !target.starts_with('.') && cdpath_redirects(env, context) {
        return None;
    }
    Some(join_path(base, target))
}

/// `(?:^|[\s;&|()'"=])(?:source|\.)[ \t]+[^\s;&|()]`.
fn has_source_command(text: &str) -> bool {
    let text = chars(text);
    let n = text.len();
    (0..n).any(|start| {
        if start > 0 && !(is_space(text[start - 1]) || ";&|()'\"=".contains(text[start - 1])) {
            return false;
        }
        let after = if text[start..].starts_with(&['s', 'o', 'u', 'r', 'c', 'e']) {
            start + 6
        } else if text[start] == '.' {
            start + 1
        } else {
            return false;
        };
        let blanks = text[after..]
            .iter()
            .take_while(|ch| **ch == ' ' || **ch == '\t')
            .count();
        blanks > 0
            && text
                .get(after + blanks)
                .is_some_and(|ch| !is_space(*ch) && !";&|()".contains(*ch))
    })
}

/// True when one command run sources a script anywhere in it (`source x`,
/// `. x`, or `source`/`.` in command position behind quoting).
pub(super) fn part_sources_scripts(part: &str, budget: &Budget) -> Scan<bool> {
    if has_source_command(part) {
        return Ok(true);
    }
    Ok(scan_words(part, budget)?
        .iter()
        .any(|word| (word.value == "source" || word.value == ".") && word.starts_command))
}

/// `re.split(r"(&&|\|\||;|\||\n)", text)`: the parts with their separators.
fn split_on_separators(text: &[char]) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < text.len() {
        let width = match (text[i], text.get(i + 1)) {
            ('&', Some('&')) | ('|', Some('|')) => 2,
            (';' | '|' | '\n', _) => 1,
            _ => 0,
        };
        if width == 0 {
            i += 1;
            continue;
        }
        parts.push(slice(text, start, i));
        parts.push(slice(text, i, i + width));
        i += width;
        start = i;
    }
    parts.push(slice(text, start, text.len()));
    parts
}

/// A `cd`/`pushd` command a part consists of.
enum CdCommand {
    /// `cd` with no argument.
    Bare,
    /// The argument text after the command word.
    Argument(String),
}

/// `cd(?:\s+(.*))?$` or `pushd\s+(.*)$` at the start of `body`, or `None`
/// when it is neither.
fn cd_command(body: &str) -> Option<CdCommand> {
    let rest_after = |name: &str| -> Option<String> {
        let rest = body.strip_prefix(name)?;
        let blanks = rest.chars().take_while(|ch| is_space(*ch)).count();
        (blanks > 0).then(|| rest.chars().skip(blanks).collect())
    };
    if body == "cd" {
        return Some(CdCommand::Bare);
    }
    rest_after("cd")
        .or_else(|| rest_after("pushd"))
        .map(CdCommand::Argument)
}

/// Per shell nesting depth (the top level plus each open subshell group):
/// whether a cd just ran, whether one ran at all, and whether the frame lost
/// track of cd success (a `;`/newline after a cd, or `||`/`|` after one).
#[derive(Clone, Copy, Default)]
struct Frame {
    pending: bool,
    saw: bool,
    poisoned: bool,
}

/// Resolve the directory a push at the end of `prefix` runs in. Subshell
/// groups run in child shells: a group that closes before the push never
/// relocates it, while an open group's cds apply to the push inside it.
pub(super) fn resolve_push_cwd(
    prefix: &str,
    user_command_start: usize,
    workspace: &str,
    env: &Assignments,
    context: &GuardContext,
    budget: &Budget,
) -> Scan<PushCwd> {
    if !(has_word(prefix, &["cd", "pushd", "popd", "source"], false)
        || part_sources_scripts(prefix, budget)?
        || prefix.contains('('))
    {
        return Ok(PushCwd::Workspace);
    }
    let mut current: Option<String> = None;
    let mut open_groups: Vec<Option<String>> = Vec::new();
    let mut frames = vec![Frame::default()];
    let mut offset = 0;
    for part in split_on_separators(&chars(prefix)) {
        let start = offset;
        offset += part.chars().count();
        if start < user_command_start {
            continue; // the configured prefix region, not model text
        }
        let top = frames.len() - 1;
        if matches!(part.as_str(), "&&" | "||" | ";" | "|" | "\n") {
            // A `;`/newline after a cd, or `||`/`|` after any: the cd may
            // or may not have succeeded, and the push's directory with it.
            if (matches!(part.as_str(), ";" | "\n") && frames[top].pending)
                || (matches!(part.as_str(), "||" | "|") && frames[top].saw)
            {
                frames[top].poisoned = true;
            }
            frames[top].pending = false;
            continue;
        }
        let trimmed = part.trim_matches(is_space);
        if trimmed.is_empty() {
            continue;
        }
        if part_sources_scripts(trimmed, budget)? || has_git_assignment(trimmed) {
            return Ok(PushCwd::Unresolvable);
        }
        let (opens, closes) = unquoted_paren_counts(trimmed);
        for _ in 0..opens {
            open_groups.push(current.clone());
            frames.push(Frame::default());
        }
        for _ in 0..closes {
            if let Some(before) = open_groups.pop() {
                current = before;
                frames.pop();
            }
        }
        if opens == closes && opens > 0 {
            continue; // a complete group: its cds are inert to what follows
        }
        let body = if open_groups.is_empty() {
            // Brace groups run in the current shell, so `{ cd sub && ...`
            // relocates like a bare cd chain.
            trimmed
                .strip_prefix('{')
                .map_or(trimmed, |rest| rest.trim_start_matches(is_space))
                .to_string()
        } else {
            trimmed
                .trim_start_matches(|ch| ch == '(' || is_space(ch))
                .trim_end_matches(|ch| ch == ')' || is_space(ch))
                .to_string()
        };
        let top = frames.len() - 1;
        let Some(cd) = cd_command(&body) else {
            if has_word(&body, &["cd", "pushd", "popd"], false) {
                return Ok(PushCwd::Unresolvable);
            }
            frames[top].pending = false;
            continue;
        };
        let arg = match cd {
            CdCommand::Argument(raw) => static_arg(raw.trim_matches(is_space)),
            CdCommand::Bare => Some(BARE_CD),
        };
        let Some(arg) = arg else {
            return Ok(PushCwd::Unresolvable);
        };
        let Some(resolved) = resolve_cd_target(&arg, current.as_deref(), workspace, env, context)
        else {
            return Ok(PushCwd::Unresolvable);
        };
        current = Some(resolved);
        frames[top].pending = true;
        frames[top].saw = true;
    }
    if frames.iter().any(|frame| frame.poisoned) {
        return Ok(PushCwd::Unresolvable);
    }
    Ok(current.map_or(PushCwd::Workspace, PushCwd::Moved))
}
