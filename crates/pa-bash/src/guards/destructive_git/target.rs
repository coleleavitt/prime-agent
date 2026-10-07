//! Where a discard's dirty-tree probe must run: `cd` chains earlier in the
//! command, `git -C <dir>` on the invocation, inline and persistent
//! `GIT_DIR`-style assignments all relocate the repository, so the probe
//! replays them; anything it cannot replay verbatim is unresolvable.

use std::sync::LazyLock;

use super::text::{
    chars, equals, mask_quoted_spans, split_whitespace_runs, starts_with, string, stripped, tokens,
    trim, unquote_one_level,
};
use super::words::{
    builtin_words, plain_word_text, prefix_holds_directory_command, reveal_shell_command_words,
    revealed_word_text, revealed_words, shell_word_positions, AliasReading, Names, ASSIGNMENT_WORD,
    REPLAYABLE_ASSIGNMENT, SHELL_KEYWORDS, TRANSPARENT_BUILTINS,
};
use crate::syntax::chars::is_space;
use crate::syntax::pyre::PyRegex;

/// The probe the guard runs when no relocation applies.
pub(super) const GIT_STATUS_PORCELAIN_COMMAND: &str =
    "git status --porcelain --untracked-files=all";

/// How the probe for one discard is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProbeTarget {
    /// The kernel's own directory and environment.
    Caller,
    /// Replay `relocation_prefix` (cd chain, assignments) before the status
    /// command.
    Relocated {
        relocation_prefix: Option<String>,
        git_status_command: String,
    },
    /// The repository the discard targets cannot be determined safely.
    Unresolvable,
}

static FUNCTION_DEFINITION: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(
        r"(?:\bfunction\s+([A-Za-z_][A-Za-z0-9_-]*)|\b([A-Za-z_][A-Za-z0-9_-]*)\s*\(\s*\))\s*\{",
    )
});
static DIRECTORY_WORD: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"\b(?:cd|pushd)\b").requiring(&["cd", "pushd"]));
static SEPARATORS: LazyLock<PyRegex> = LazyLock::new(|| PyRegex::new(r"(&&|\|\||;|\||\n)"));
static CORE_RELOCATION: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"core\.(worktree|bare)(=|$)"));
static TRAP_REPORT_OPTION: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"-[A-Za-z]*[pl][A-Za-z]*"));

/// Index just past the `}` closing the `{` at `open_index`.
fn brace_group_end(text: &[char], open_index: usize) -> usize {
    let mut depth = 0i64;
    for (index, ch) in text.iter().enumerate().skip(open_index) {
        if *ch == '{' {
            depth += 1;
        } else if *ch == '}' {
            depth -= 1;
            if depth == 0 {
                return index + 1;
            }
        }
    }
    text.len()
}

/// Whether `prefix` defines a function whose body can change directory (the
/// guard does not model invocation, so the definition counts as if it ran).
fn defines_directory_changing_function(prefix: &[char]) -> bool {
    let masked = mask_quoted_spans(prefix);
    for found in FUNCTION_DEFINITION.find_all(&masked) {
        let body_end = brace_group_end(&masked, found.end() - 1) - 1;
        let body_start = found.end();
        if body_start <= body_end && DIRECTORY_WORD.is_found(&masked[body_start..body_end]) {
            return true;
        }
        let body = if body_start <= body_end {
            stripped(&prefix[body_start..body_end])
        } else {
            Vec::new()
        };
        for word in shell_word_positions(&body) {
            if word.command {
                if let Some(plain) = plain_word_text(&body[word.start..word.end]) {
                    if plain == "cd" || plain == "pushd" {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Whether `prefix` defines a function named `git`, shadowing the discard.
fn defines_git_shadowing_function(prefix: &[char]) -> bool {
    let masked = mask_quoted_spans(prefix);
    FUNCTION_DEFINITION.find_all(&masked).iter().any(|found| {
        let name = found.group(1).or_else(|| found.group(2)).unwrap_or((0, 0));
        plain_word_text(&stripped(&masked[name.0..name.1])).as_deref() == Some("git")
    })
}

/// Whether `prefix` installs a trap whose action can change directory (a
/// trap action runs in the installing shell, at a time the guard does not
/// model).
fn installs_relocating_trap(prefix: &[char]) -> bool {
    let none = Names::new();
    let masked = mask_quoted_spans(prefix);
    let known =
        reveal_shell_command_words(prefix, AliasReading::Expanded, &none, &none).assignments;
    let (written, revealed, rebuilt) = revealed_words(prefix, Some(&known));
    for (index, word) in shell_word_positions(&rebuilt).iter().enumerate() {
        if !word.command || revealed.get(index).map(String::as_str) != Some("trap") {
            continue;
        }
        let Some(trap) = written.get(index) else {
            break;
        };
        let region_end = (trap.end..masked.len())
            .find(|&j| ";&|\n".contains(masked[j]))
            .unwrap_or(prefix.len());
        let start = trap.end;
        let region = &prefix[start..region_end.max(start)];
        for candidate in shell_word_positions(region) {
            let raw = &region[candidate.start..candidate.end];
            let option = revealed_word_text(raw, None);
            if option.starts_with('-') {
                if TRAP_REPORT_OPTION.is_full_match(&chars(&option)) {
                    break;
                }
                continue;
            }
            if prefix_holds_directory_command(&unquote_one_level(raw)) {
                return true;
            }
            break;
        }
    }
    false
}

/// The directory builtin a segment's command word runs.
enum DirectoryCommand {
    /// `(prefix words replayed verbatim, cd|pushd, arguments as written)`.
    Runs(String, String, Vec<char>),
    /// A word the replay would need cannot be replayed (or `!` precedes it).
    Unresolvable,
    /// The segment runs no directory builtin.
    None,
}

fn directory_command_parts(segment: &[char]) -> DirectoryCommand {
    let (written, revealed, rebuilt) = revealed_words(segment, None);
    let mut prefix: Vec<String> = Vec::new();
    let mut negated = false;
    for (index, word) in shell_word_positions(&rebuilt).iter().enumerate() {
        if !word.command {
            continue;
        }
        let (Some(spot), Some(plain)) = (written.get(index), revealed.get(index)) else {
            break;
        };
        let raw = &segment[spot.start..spot.end];
        if plain == "cd" || plain == "pushd" {
            if negated {
                return DirectoryCommand::Unresolvable;
            }
            return DirectoryCommand::Runs(
                prefix.join(" "),
                plain.clone(),
                segment[spot.end..].to_vec(),
            );
        }
        if TRANSPARENT_BUILTINS.contains(&plain.as_str()) {
            prefix.push(string(raw));
        } else if plain == "!" {
            negated = true;
        } else if equals(raw, plain) && SHELL_KEYWORDS.contains(&plain.as_str()) {
            // Shell syntax: the command word still follows.
        } else if ASSIGNMENT_WORD.is_full_match(raw) {
            if !REPLAYABLE_ASSIGNMENT.is_full_match(raw) {
                return DirectoryCommand::Unresolvable;
            }
            prefix.push(string(raw));
        } else {
            return DirectoryCommand::None;
        }
    }
    DirectoryCommand::None
}

/// The `cd` command the probe replays for one entry of a cd chain.
fn directory_replay(prefix: &str, arguments: &str) -> String {
    [prefix, "cd", arguments]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn has_any(text: &[char], set: &str) -> bool {
    text.iter().any(|ch| set.contains(*ch))
}

/// Strip leading `(`/whitespace and trailing `)`/whitespace (a grouped
/// segment's body).
fn group_body(trimmed: &[char]) -> &[char] {
    let start = trimmed
        .iter()
        .position(|ch| *ch != '(' && !is_space(*ch))
        .unwrap_or(trimmed.len());
    let rest = &trimmed[start..];
    let end = rest
        .iter()
        .rposition(|ch| *ch != ')' && !is_space(*ch))
        .map_or(0, |end| end + 1);
    &rest[..end]
}

/// Drop a leading `{` and the whitespace after it.
fn without_open_brace(trimmed: &[char]) -> &[char] {
    if trimmed.first() == Some(&'{') {
        let rest = &trimmed[1..];
        let skip = rest.iter().take_while(|ch| is_space(**ch)).count();
        &rest[skip..]
    } else {
        trimmed
    }
}

/// Drop leading `{ ` and `then `/`do `/`else ` openers (bodies that run in
/// the current shell).
fn without_openers(mut text: &[char]) -> &[char] {
    loop {
        if text.first() == Some(&'{') {
            text = without_open_brace(text);
            continue;
        }
        let keyword = ["then", "do", "else"].into_iter().find(|keyword| {
            starts_with(text, keyword) && text.get(keyword.len()).is_some_and(|ch| is_space(*ch))
        });
        let Some(keyword) = keyword else { return text };
        let rest = &text[keyword.len()..];
        let skip = rest.iter().take_while(|ch| is_space(**ch)).count();
        text = &rest[skip..];
    }
}

/// The value of a `-C`/`-c`-style option: attached, or the next token.
fn option_value<'a>(tokens: &[&'a [char]], index: usize, flag: &str) -> Option<&'a [char]> {
    let token = tokens[index];
    if equals(token, flag) {
        tokens.get(index + 1).copied()
    } else {
        Some(&token[flag.chars().count()..])
    }
}

/// Resolve where the discard whose `git` word starts at `discard_index` in
/// `command` must be probed. `user_command_start` is where the user's command
/// begins after the trusted prefix (0 without one).
#[expect(
    clippy::too_many_lines,
    reason = "one pass mirroring the shell's reading of the prefix"
)]
pub(super) fn resolve_probe_target(
    command: &[char],
    discard_index: usize,
    user_command_start: usize,
) -> ProbeTarget {
    let prefix = &command[..discard_index];
    let invocation = &command[discard_index..];
    if user_command_start > 0 && discard_index < user_command_start {
        return ProbeTarget::Unresolvable;
    }
    let tokens_all = split_whitespace_runs(invocation);
    let mut dash_c_dir: Option<String> = None;
    let mut subcommand_index: Option<usize> = None;
    for (index, token) in tokens_all.iter().enumerate().skip(1) {
        if ["reset", "checkout", "clean", "restore"]
            .iter()
            .any(|name| equals(token, name))
        {
            subcommand_index = Some(index);
            break;
        }
        if equals(token, "-C") || (starts_with(token, "-C") && token.len() > 2) {
            let directory = option_value(&tokens_all, index, "-C");
            let Some(directory) =
                directory.filter(|dir| !dir.is_empty() && !has_any(dir, "\"'\\$`"))
            else {
                return ProbeTarget::Unresolvable;
            };
            let directory = string(directory);
            dash_c_dir = Some(match dash_c_dir {
                Some(previous) => format!("{previous} -C {directory}"),
                None => directory,
            });
        } else if ["--git-dir", "--work-tree", "--prefix"]
            .iter()
            .any(|option| starts_with(token, option))
        {
            return ProbeTarget::Unresolvable;
        } else if equals(token, "-c") || (starts_with(token, "-c") && token.len() > 2) {
            let config = option_value(&tokens_all, index, "-c");
            if config.is_some_and(|config| {
                !config.is_empty() && CORE_RELOCATION.match_start(config).is_some()
            }) {
                return ProbeTarget::Unresolvable;
            }
        } else if starts_with(token, "--config-env") {
            let config = if !equals(token, "--config-env") && token.contains(&'=') {
                let at = token.iter().position(|ch| *ch == '=').unwrap_or(0);
                Some(&token[at + 1..])
            } else {
                tokens_all.get(index + 1).copied()
            };
            if config.is_some_and(|config| {
                !config.is_empty() && CORE_RELOCATION.match_start(config).is_some()
            }) {
                return ProbeTarget::Unresolvable;
            }
        } else if starts_with(token, "-") && !starts_with(token, "--") && has_any(&token[1..], "Cc")
        {
            return ProbeTarget::Unresolvable;
        }
    }

    let mut clean_removes_ignored = false;
    if let Some(index) = subcommand_index.filter(|&index| equals(tokens_all[index], "clean")) {
        for token in &tokens_all[index + 1..] {
            if equals(token, "--") {
                break;
            }
            if starts_with(token, "--") {
                continue;
            }
            if starts_with(token, "-") && has_any(&token[1..], "xX") {
                clean_removes_ignored = true;
                break;
            }
        }
    }

    // Inline assignments directly before the git word relocate the target.
    let parts = SEPARATORS.split_keeping(prefix);
    let segments: Vec<&Vec<char>> = parts.iter().step_by(2).collect();
    let last_segment = segments[segments.len() - 1];
    let leading_tokens = tokens(trim(last_segment));
    for token in &leading_tokens {
        if REPLAYABLE_ASSIGNMENT.is_full_match(token) {
            continue;
        }
        if ["sudo", "env", "command", "builtin"]
            .iter()
            .any(|word| equals(token, word))
            || token.last() == Some(&'/')
        {
            continue;
        }
        return ProbeTarget::Unresolvable;
    }
    let assignments: Vec<String> = leading_tokens
        .iter()
        .filter(|token| token.contains(&'='))
        .map(|token| string(token))
        .collect();
    let mut persistent_assignments: Vec<String> = Vec::new();
    if segments.len() > 1 {
        let mut positions = Vec::new();
        let mut offset = 0;
        for (index, part) in parts.iter().enumerate() {
            if index % 2 == 0 {
                positions.push(offset);
            }
            offset += part.len();
        }
        for index in 0..segments.len() - 1 {
            if positions[index] < user_command_start {
                continue;
            }
            let segment = without_openers(trim(segments[index]));
            let seg_tokens = tokens(segment);
            if seg_tokens
                .first()
                .is_some_and(|first| equals(first, "source") || equals(first, "."))
            {
                return ProbeTarget::Unresolvable;
            }
            let separator = string(&parts[2 * index + 1]);
            let removal = builtin_words(&seg_tokens);
            if let Some(head) = removal.first() {
                if revealed_word_text(head, None) == "unset"
                    && separator != "|"
                    && removal[1..].iter().any(|token| {
                        plain_word_text(token).is_some_and(|plain| plain.starts_with("GIT_"))
                    })
                {
                    return ProbeTarget::Unresolvable;
                }
            }
            if ![";", "&&", "\n"].contains(&separator.as_str()) || seg_tokens.is_empty() {
                continue;
            }
            if equals(seg_tokens[0], "export") {
                let rest = &seg_tokens[1..];
                if rest.is_empty()
                    || !rest
                        .iter()
                        .all(|token| REPLAYABLE_ASSIGNMENT.is_full_match(token))
                {
                    return ProbeTarget::Unresolvable;
                }
                persistent_assignments.extend(rest.iter().map(|token| string(token)));
            } else if seg_tokens
                .iter()
                .all(|token| REPLAYABLE_ASSIGNMENT.is_full_match(token))
            {
                persistent_assignments.extend(seg_tokens.iter().map(|token| string(token)));
            }
        }
    }
    let env_prefix = if persistent_assignments.is_empty() && assignments.is_empty() {
        String::new()
    } else {
        format!(
            "{} ",
            persistent_assignments
                .iter()
                .chain(&assignments)
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        )
    };

    if defines_directory_changing_function(prefix)
        || defines_git_shadowing_function(prefix)
        || installs_relocating_trap(prefix)
    {
        return ProbeTarget::Unresolvable;
    }

    let mut persistent_cd_commands: Vec<String> = Vec::new();
    let mut grouped_cd_commands: Vec<String> = Vec::new();
    let mut saw_cd = false;
    let mut paren_depth: usize = 0;
    let mut cd_pending_separator = false;
    if prefix_holds_directory_command(prefix) || prefix.contains(&'(') {
        let mut offset = 0;
        for part in &parts {
            let start = offset;
            offset += part.len();
            if start < user_command_start {
                continue;
            }
            let part_text = string(part);
            if ["&&", "||", ";", "|", "\n"].contains(&part_text.as_str()) {
                if cd_pending_separator && (part_text == ";" || part_text == "\n") {
                    return ProbeTarget::Unresolvable;
                }
                if part_text == "||" || part_text == "|" {
                    if saw_cd {
                        return ProbeTarget::Unresolvable;
                    }
                    continue;
                }
                cd_pending_separator = false;
                continue;
            }
            let trimmed = trim(part);
            let opens = part.iter().filter(|ch| **ch == '(').count();
            let closes = part.iter().filter(|ch| **ch == ')').count();
            let inside_group = paren_depth > 0 || opens > 0;
            paren_depth = (paren_depth + opens).saturating_sub(closes);
            if inside_group {
                let body = group_body(trimmed);
                match directory_command_parts(body) {
                    DirectoryCommand::Unresolvable => return ProbeTarget::Unresolvable,
                    DirectoryCommand::Runs(prefix_text, name, arguments) => {
                        if name == "pushd" {
                            return ProbeTarget::Unresolvable;
                        }
                        let arg = trim(&arguments);
                        if arg.is_empty() || has_any(arg, "$`;&|()<>#\"") {
                            return ProbeTarget::Unresolvable;
                        }
                        saw_cd = true;
                        cd_pending_separator = true;
                        grouped_cd_commands.push(directory_replay(&prefix_text, &string(arg)));
                    }
                    DirectoryCommand::None => {
                        if DIRECTORY_WORD.is_found(trimmed) {
                            return ProbeTarget::Unresolvable;
                        }
                    }
                }
                if paren_depth == 0 {
                    grouped_cd_commands.clear();
                }
                continue;
            }
            let group_free = without_open_brace(trimmed);
            let (prefix_text, name, arguments) = match directory_command_parts(group_free) {
                DirectoryCommand::Unresolvable => return ProbeTarget::Unresolvable,
                DirectoryCommand::None => {
                    cd_pending_separator = false;
                    continue;
                }
                DirectoryCommand::Runs(prefix_text, name, arguments) => {
                    (prefix_text, name, arguments)
                }
            };
            if name == "pushd" {
                return ProbeTarget::Unresolvable;
            }
            let arg = trim(&arguments);
            let quotes = |quote: char| arg.iter().filter(|ch| **ch == quote).count();
            let balanced = quotes('"') % 2 == 0 && quotes('\'') % 2 == 0;
            if !balanced || (!arg.is_empty() && has_any(arg, "$`;&|()<>#")) {
                return ProbeTarget::Unresolvable;
            }
            saw_cd = true;
            cd_pending_separator = true;
            persistent_cd_commands.push(directory_replay(&prefix_text, &string(arg)));
        }
    }

    let mut cd_commands = persistent_cd_commands;
    if paren_depth > 0 {
        cd_commands.extend(grouped_cd_commands);
    }
    if cd_commands.is_empty()
        && dash_c_dir.is_none()
        && !clean_removes_ignored
        && env_prefix.is_empty()
    {
        return ProbeTarget::Caller;
    }
    let ignored = if clean_removes_ignored {
        " --ignored=matching"
    } else {
        ""
    };
    let cd_prefix = if cd_commands.is_empty() {
        String::new()
    } else {
        format!("{} && ", cd_commands.join(" && "))
    };
    let git_status = match dash_c_dir.as_deref().filter(|dir| !dir.is_empty()) {
        Some(dir) => format!("git -C {dir} status --porcelain --untracked-files=all{ignored}"),
        None => format!("git status --porcelain --untracked-files=all{ignored}"),
    };
    let statement_prefix = if !persistent_assignments.is_empty() && !cd_commands.is_empty() {
        format!("{} && ", persistent_assignments.join(" && "))
    } else {
        String::new()
    };
    let relocation = format!("{statement_prefix}{cd_prefix}{env_prefix}");
    ProbeTarget::Relocated {
        relocation_prefix: (!relocation.is_empty()).then_some(relocation),
        git_status_command: git_status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(command: &str) -> ProbeTarget {
        let text = chars(command);
        let at = command
            .rfind("git ")
            .map_or(0, |byte| command[..byte].chars().count());
        resolve_probe_target(&text, at, 0)
    }

    #[test]
    fn cd_chains_and_dash_c_relocate_the_probe() {
        assert_eq!(target("git reset --hard"), ProbeTarget::Caller);
        assert_eq!(
            target("cd sub && git reset --hard"),
            ProbeTarget::Relocated {
                relocation_prefix: Some("cd sub && ".to_string()),
                git_status_command: GIT_STATUS_PORCELAIN_COMMAND.to_string(),
            }
        );
        assert_eq!(
            target("git -C a -C b clean -fx"),
            ProbeTarget::Relocated {
                relocation_prefix: None,
                git_status_command:
                    "git -C a -C b status --porcelain --untracked-files=all --ignored=matching"
                        .to_string(),
            }
        );
        assert_eq!(
            target("cd sub; git reset --hard"),
            ProbeTarget::Unresolvable
        );
        assert_eq!(
            target("cd $X && git reset --hard"),
            ProbeTarget::Unresolvable
        );
    }
}
