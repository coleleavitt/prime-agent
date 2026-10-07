//! Where a recursive chmod/chown acts: the kernel workspace and home, the
//! directory statically-known `cd` relocations move the shell to, and the
//! absolute path each operand resolves to.

use super::invocations::region_words;
use super::normalize::expand_ansi_c_payloads;
use super::patterns::{assigns_anywhere, has_glob_or_substitution, has_word};
use super::pyos::{cwd_text, expanduser, is_abs, is_file, join, realpath, strip};
use super::words::ShellWord;
use crate::context::GuardContext;

/// The fixed locations one check measures against.
#[derive(Debug, Clone)]
pub(super) struct Locations<'a> {
    pub context: &'a GuardContext,
    /// The realpath of the kernel's working directory.
    pub workspace: String,
    /// `HOME` when set and non-empty.
    pub home_env: Option<String>,
    /// The realpath of `HOME`.
    pub home_real: Option<String>,
}

impl<'a> Locations<'a> {
    pub(super) fn new(context: &'a GuardContext) -> Self {
        let cwd = cwd_text(context);
        let workspace = realpath(context, &cwd).unwrap_or(cwd);
        let home_env = context
            .var("HOME")
            .filter(|home| !home.is_empty())
            .map(str::to_string);
        let home_real = home_env.as_deref().and_then(|home| realpath(context, home));
        Self {
            context,
            workspace,
            home_env,
            home_real,
        }
    }

    fn cdpath_set(&self) -> bool {
        self.context
            .var("CDPATH")
            .is_some_and(|value| !value.is_empty())
    }
}

/// The directory a command runs in after the relocations before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EffectiveCwd {
    /// No cd moved the shell: the kernel workspace.
    Workspace,
    Dir(String),
    /// Something relocates in a way the guard cannot replay.
    Unresolvable,
}

/// Split on `&&`, `||`, `;`, `|` and newlines, keeping the separators
/// (`re.split` with a capture group).
fn split_keeping_separators(text: &[char]) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut i = 0;
    while i < text.len() {
        let pair = text.get(i + 1).map(|next| (text[i], *next));
        let separator = match pair {
            Some(('&', '&')) => Some("&&"),
            Some(('|', '|')) => Some("||"),
            _ => match text[i] {
                ';' => Some(";"),
                '|' => Some("|"),
                '\n' => Some("\n"),
                _ => None,
            },
        };
        if let Some(separator) = separator {
            parts.push(std::mem::take(&mut current));
            parts.push(separator.to_string());
            i += separator.len();
        } else {
            current.push(text[i]);
            i += 1;
        }
    }
    parts.push(current);
    parts
}

/// `re.match(r"cd\s*(.*)$", text)`: the stripped argument of a leading `cd`.
fn cd_argument(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("cd")?;
    Some(strip(rest.trim_start_matches(super::pyos::is_space)))
}

const RELOCATORS: [&str; 3] = ["cd", "pushd", "popd"];

/// Replay the statically-known `cd` relocations in `prefix` (the command
/// text before the invocation): parens groups run in subshells, brace
/// groups in the current shell; anything unreplayable is
/// [`EffectiveCwd::Unresolvable`]. The region before `user_command_start`
/// is the trusted command prefix and is skipped.
pub(super) fn effective_cwd(
    locations: &Locations<'_>,
    prefix: &[char],
    user_command_start: usize,
) -> EffectiveCwd {
    let text: String = prefix.iter().collect();
    if !(has_word(&text, &RELOCATORS) || text.contains('(')) {
        return EffectiveCwd::Workspace;
    }
    let mut current: Option<String> = None;
    let mut cdpath_armed = locations.cdpath_set();
    let mut open_groups: Vec<Option<String>> = Vec::new();
    let mut paren_depth = 0usize;
    let mut saw_cd = false;
    let mut cd_pending_separator = false;
    let mut offset = 0usize;
    for part in split_keeping_separators(prefix) {
        let start = offset;
        offset += part.chars().count();
        if start < user_command_start {
            continue;
        }
        if matches!(part.as_str(), "&&" | "||" | ";" | "|" | "\n") {
            if cd_pending_separator && (part == ";" || part == "\n") {
                return EffectiveCwd::Unresolvable;
            }
            if (part == "||" || part == "|") && saw_cd {
                return EffectiveCwd::Unresolvable;
            }
            cd_pending_separator = false;
            continue;
        }
        let trimmed = strip(&part);
        if assigns_anywhere(trimmed, "CDPATH") {
            cdpath_armed = true;
        }
        let opens = part.matches('(').count();
        let closes = part.matches(')').count();
        let inside_group = paren_depth > 0 || opens > 0;
        open_groups.extend(std::iter::repeat_n(current.clone(), opens));
        paren_depth = (paren_depth + opens).saturating_sub(closes);
        if inside_group {
            let body = trimmed
                .trim_start_matches(|c: char| c == '(' || super::pyos::is_space(c))
                .trim_end_matches(|c: char| c == ')' || super::pyos::is_space(c));
            if let Some(argument) = cd_argument(body) {
                match relocate(locations, argument, current.as_deref(), cdpath_armed) {
                    Some(resolved) => current = Some(resolved),
                    None => return EffectiveCwd::Unresolvable,
                }
                saw_cd = true;
                cd_pending_separator = true;
            } else if has_word(trimmed, &RELOCATORS) {
                return EffectiveCwd::Unresolvable;
            }
            if paren_depth == 0 {
                if let Some(restored) = open_groups.pop() {
                    current = restored;
                }
            }
            continue;
        }
        let group_free = trimmed.strip_prefix('{').map_or(trimmed, |rest| {
            rest.trim_start_matches(super::pyos::is_space)
        });
        let Some(argument) = cd_argument(group_free) else {
            if has_word(group_free, &RELOCATORS) {
                return EffectiveCwd::Unresolvable;
            }
            cd_pending_separator = false;
            continue;
        };
        match relocate(locations, argument, current.as_deref(), cdpath_armed) {
            Some(resolved) => current = Some(resolved),
            None => return EffectiveCwd::Unresolvable,
        }
        saw_cd = true;
        cd_pending_separator = true;
    }
    current.map_or(EffectiveCwd::Workspace, EffectiveCwd::Dir)
}

/// Resolve one raw `cd` argument, or `None` when it cannot be replayed.
fn relocate(
    locations: &Locations<'_>,
    raw: &str,
    current: Option<&str>,
    cdpath_armed: bool,
) -> Option<String> {
    let argument = statically_resolvable_cd_arg(raw)?;
    cd_target(locations, &argument, current, cdpath_armed)
}

/// Unquote one cd argument to its literal path, or `None` when it is empty,
/// multi-word, carries shell syntax, or ends mid-quote.
fn statically_resolvable_cd_arg(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let expanded = expand_ansi_c_payloads(&raw.chars().collect::<Vec<_>>());
    if expanded.iter().any(|c| "$`;&|()<>#".contains(*c)) {
        return None;
    }
    let (words, well_formed) = region_words(&expanded);
    match words.as_slice() {
        [Some(word)] if well_formed && !word.is_empty() => Some(word.clone()),
        _ => None,
    }
}

/// Where `cd <argument>` lands from `current` (`None` = the workspace),
/// logical like `cd -L`; `None` for options, another user's home, or a
/// relative target CDPATH could redirect.
fn cd_target(
    locations: &Locations<'_>,
    argument: &str,
    current: Option<&str>,
    cdpath_armed: bool,
) -> Option<String> {
    if argument.is_empty() {
        return Some(expanduser(locations.context, "~"));
    }
    if argument.starts_with('-') {
        return None;
    }
    if argument.starts_with('~') {
        if argument == "~" || argument.starts_with("~/") {
            return Some(expanduser(locations.context, argument));
        }
        return None;
    }
    let cdpath_exempt = argument == "."
        || argument == ".."
        || argument.starts_with("./")
        || argument.starts_with("../");
    if !is_abs(argument) && !cdpath_exempt && (cdpath_armed || locations.cdpath_set()) {
        return None;
    }
    if is_abs(argument) {
        Some(argument.to_string())
    } else {
        Some(join(current.unwrap_or(&locations.workspace), argument))
    }
}

/// The absolute path one operand acts on (`~`, `$HOME`, `$PWD` expanded,
/// realpath'd), or `None` when it cannot be resolved statically.
pub(super) fn resolve_operand(locations: &Locations<'_>, text: &str, base: &str) -> Option<String> {
    let mut path = text.to_string();
    if path.starts_with('~') {
        if !(path == "~" || path.starts_with("~/")) {
            return None;
        }
        path = expanduser(locations.context, &path);
        if path.is_empty() || path.starts_with('~') {
            return None;
        }
    }
    match &locations.home_env {
        Some(home) => path = path.replace("${HOME}", home).replace("$HOME", home),
        None if path.contains("${HOME}") || path.contains("$HOME") => return None,
        None => {}
    }
    path = path.replace("${PWD}", base).replace("$PWD", base);
    if path.is_empty() || has_glob_or_substitution(&path) {
        return None;
    }
    let candidate = if is_abs(&path) {
        path
    } else {
        join(base, &path)
    };
    realpath(locations.context, &candidate)
}

/// Whether `resolved` is the workspace or under it.
fn inside(resolved: &str, workspace: &str) -> bool {
    resolved == workspace || resolved.starts_with(&format!("{workspace}/"))
}

/// Why a resolved operand must be refused, or `None` when it is safe.
pub(super) fn operand_violation(
    locations: &Locations<'_>,
    resolved: Option<&str>,
) -> Option<&'static str> {
    let Some(resolved) = resolved else {
        return Some("cannot be resolved statically (glob, substitution, or quotes)");
    };
    let workspace = locations.workspace.as_str();
    if resolved == "/" {
        return Some("names the filesystem root (/)");
    }
    if locations.home_real.as_deref() == Some(resolved) {
        return Some("names the home directory");
    }
    if !inside(resolved, workspace) {
        return Some("escapes the kernel workspace");
    }
    if resolved != workspace
        && resolved[workspace.len() + 1..]
            .split('/')
            .any(|component| component.starts_with('.'))
    {
        return Some("names a dot-directory or dotfile (e.g. .git)");
    }
    None
}

/// Whether a resolved script input must be refused (location policy only:
/// dot-components inside the workspace are legitimate scripts).
pub(super) fn script_input_violation(locations: &Locations<'_>, resolved: Option<&str>) -> bool {
    match resolved {
        None => true,
        Some(resolved) => {
            resolved == "/"
                || locations.home_real.as_deref() == Some(resolved)
                || !inside(resolved, &locations.workspace)
        }
    }
}

/// A command word with its statically-known `$HOME`/`$PWD` forms expanded.
pub(super) fn expanded_command_word_value(locations: &Locations<'_>, value: &str) -> String {
    let mut expanded = value.to_string();
    match &locations.home_env {
        Some(home) => expanded = expanded.replace("${HOME}", home).replace("$HOME", home),
        None if expanded.contains("${HOME}") || expanded.contains("$HOME") => return expanded,
        None => {}
    }
    let cwd = cwd_text(locations.context);
    expanded.replace("${PWD}", &cwd).replace("$PWD", &cwd)
}

/// Whether the PATH this command runs under can resolve a bare command word
/// inside a directory the guard cannot trust (an empty, relative or
/// workspace entry, in the assigned or the inherited value).
pub(super) fn path_can_shadow_command_lookup(
    locations: &Locations<'_>,
    words: &[ShellWord],
) -> bool {
    let value_of = |word: &ShellWord| {
        word.value
            .split_once('=')
            .map_or(String::new(), |(_, value)| value.to_string())
    };
    let set_values: Vec<String> = words
        .iter()
        .filter(|word| word.value.starts_with("PATH="))
        .map(value_of)
        .collect();
    let appended = words
        .iter()
        .filter(|word| super::patterns::assigns(&word.value, "PATH"))
        .map(value_of);
    let mut checked: Vec<String> = match set_values.last() {
        Some(last) => vec![last.clone()],
        None => vec![locations
            .context
            .var("PATH")
            .unwrap_or_default()
            .to_string()],
    };
    checked.extend(appended);
    checked.iter().any(|value| {
        expanded_command_word_value(locations, value)
            .split(':')
            .any(|entry| {
                if entry.is_empty() || !is_abs(entry) {
                    return true;
                }
                realpath(locations.context, entry)
                    .is_none_or(|resolved| inside(&resolved, &locations.workspace))
            })
    })
}

/// The first `PATH` entry holding `candidate` as a file.
pub(super) fn path_hit(locations: &Locations<'_>, candidate: &str) -> Option<String> {
    locations
        .context
        .var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| join(dir, candidate))
        .find(|hit| is_file(locations.context, hit))
}

/// Whether a `PATH` assignment appears anywhere in the text.
pub(super) fn assigns_path(normalized: &[char]) -> bool {
    assigns_anywhere(&normalized.iter().collect::<String>(), "PATH")
}
