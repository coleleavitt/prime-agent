//! Finding the destructive git discards in a command: `git checkout -- .`,
//! `git restore .` (worktree), `git reset --hard`, and every `git clean` but a
//! dry run. Conservative by design: a false positive costs one `git status`
//! probe, a false negative silently loses work.

use std::sync::LazyLock;

use super::text::{
    contains, equals, mask_quoted_spans, normalized, split_whitespace_runs, starts_with,
};
use super::words::{reveal_shell_command_words, AliasReading, Names};
use crate::syntax::pyre::PyRegex;

/// One git option token and the separate value word that may follow it,
/// written as disjoint shapes so every token has exactly one reading and the
/// scan stays linear (a repeated `-x` must not re-partition exponentially).
const GIT_OPTION_TOKEN: &str = r"-(?:-[^\s;&|]*|[^-\s;&|][^\s;&|]*)";
const GIT_OPTION_VALUE: &str = r#"(?:"[^"]*"|'[^']*'|[^-\s;&|][^\s;&|]*)"#;
/// A pathspec read from a file names any path, `.` and `:/` included.
const PATHSPEC_FROM_FILE: &str = r"--pathspec-from-file(?:=\S+|\s+\S+)";

fn global_options() -> String {
    format!(r"(?:{GIT_OPTION_TOKEN}(?:\s+{GIT_OPTION_VALUE})?\s+)*")
}

static CHECKOUT: LazyLock<PyRegex> = LazyLock::new(|| {
    let options = global_options();
    PyRegex::new(&format!(
        r"\bgit\s+{options}checkout\s+(?:(?:(?:-[fm]|--ours|--theirs|--conflict=\S+)\s+)*(?:(?:--\s+)?(?:\./?|:/)|{PATHSPEC_FROM_FILE})|[^\s;&|()]+\s+(?:(?:--\s+)?(?:\./?|:/)|{PATHSPEC_FROM_FILE})|(?:-f|--force)\s+[^\s;&|()]+)(?=\s|$|[;&|)])"
    ))
    .requiring(&["git"])
});
static RESTORE: LazyLock<PyRegex> = LazyLock::new(|| {
    let options = global_options();
    PyRegex::new(&format!(
        r"\bgit\s+{options}restore\s+((?:{GIT_OPTION_TOKEN}(?:\s+{GIT_OPTION_VALUE})?\s+)*)(?:{PATHSPEC_FROM_FILE}|\./?|:/)(?=\s|$|[;&|)])"
    ))
    .requiring(&["git"])
});
static RESET: LazyLock<PyRegex> = LazyLock::new(|| {
    let options = global_options();
    PyRegex::new(&format!(
        r"\bgit\s+{options}reset\s+(?:(?:-[^\s;&|]+)\s+)*--hard\b"
    ))
    .requiring(&["git"])
});
static CLEAN: LazyLock<PyRegex> = LazyLock::new(|| {
    let options = global_options();
    PyRegex::new(&format!(
        r"\bgit\s+{options}clean(?=\s|$|[;&|)])([^;&|\n]*)"
    ))
    .requiring(&["git"])
});

/// `git restore` targets the working tree unless `--staged`/`-S` alone is
/// given; the tree-ish of `-s`/`--source` is data, not a flag cluster.
fn restore_options_discard_worktree(option_region: &[char]) -> bool {
    let mut worktree = false;
    let mut staged = false;
    let mut source_value_next = false;
    for token in split_whitespace_runs(option_region) {
        if token.is_empty() {
            continue;
        }
        if source_value_next {
            source_value_next = false;
            continue;
        }
        if equals(token, "--") {
            break;
        }
        if starts_with(token, "--") {
            if starts_with(token, "--worktree") {
                worktree = true;
            } else if starts_with(token, "--staged") {
                staged = true;
            } else if equals(token, "--source") {
                source_value_next = true;
            }
            continue;
        }
        let mut flags = &token[1..];
        if let Some(index) = flags.iter().position(|ch| *ch == 's') {
            source_value_next = index == flags.len() - 1;
            flags = &flags[..index];
        }
        if flags.contains(&'W') {
            worktree = true;
        }
        if flags.contains(&'S') {
            staged = true;
        }
    }
    worktree || !staged
}

/// Whether a `git clean` argument region can delete untracked files: every
/// segment but a dry run (the force requirement can be configured away).
fn is_destructive_clean_segment(args: &[char]) -> bool {
    let tokens: Vec<&[char]> = split_whitespace_runs(args)
        .into_iter()
        .filter(|token| !token.is_empty())
        .collect();
    let options = match tokens.iter().position(|token| equals(token, "--")) {
        Some(end) => &tokens[..end],
        None => &tokens[..],
    };
    !options.iter().any(|token| {
        equals(token, "--dry-run")
            || (starts_with(token, "-") && !starts_with(token, "--") && token.contains(&'n'))
    })
}

/// One discard found in a scanned command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct DiscardSite {
    /// Where the `git` word starts in the scanned text.
    pub index: usize,
    /// The discard appears only through a revealed value that holds more than
    /// a bare executable word: the repository it targets cannot be named.
    pub revealed: bool,
}

fn scan_discard_sites(
    normalized: &[char],
    index_map: &[usize],
    reading: AliasReading,
    aliases: &Names,
    assignments: &Names,
) -> Vec<DiscardSite> {
    let revealed = reveal_shell_command_words(normalized, reading, aliases, assignments);
    let masked = mask_quoted_spans(&revealed.text);
    let mut matches: Vec<(usize, usize)> = Vec::new();
    for pattern in [&*CHECKOUT, &*RESET] {
        matches.extend(
            pattern
                .find_all(&masked)
                .iter()
                .map(|found| (found.start(), found.end())),
        );
    }
    for found in RESTORE.find_all(&masked) {
        let (start, end) = found.group(1).unwrap_or((found.start(), found.start()));
        if restore_options_discard_worktree(&masked[start..end]) {
            matches.push((found.start(), found.end()));
        }
    }
    for found in CLEAN.find_all(&masked) {
        let (start, end) = found.group(1).unwrap_or((found.end(), found.end()));
        if is_destructive_clean_segment(&masked[start..end]) {
            matches.push((found.start(), found.end()));
        }
    }
    matches.sort_unstable();
    matches
        .into_iter()
        .map(|(start, end)| DiscardSite {
            index: index_map[revealed.index_map[start]],
            revealed: (start..end)
                .any(|index| revealed.unnameable.contains(&revealed.index_map[index])),
        })
        .collect()
}

/// Every destructive discard in `command`, by where its `git` word starts.
/// `aliases`/`assignments` seed names a caller already knows (text the shell
/// parses later, an `eval` payload, resolves a name the outer text defined).
/// Alias definitions in the text are read both expanded and as written: the
/// shell's options decide, and the scan cannot see them.
pub(super) fn find_discard_sites(
    command: &[char],
    aliases: &Names,
    assignments: &Names,
) -> Vec<DiscardSite> {
    let (normalized, index_map) = normalized(command);
    let mut sites = scan_discard_sites(
        &normalized,
        &index_map,
        AliasReading::Expanded,
        aliases,
        assignments,
    );
    if contains(&normalized, "alias") {
        sites.extend(scan_discard_sites(
            &normalized,
            &index_map,
            AliasReading::AsWritten,
            aliases,
            assignments,
        ));
    }
    sites.sort_by_key(|site| site.index);
    let mut unique: Vec<DiscardSite> = Vec::new();
    for site in sites {
        if !unique.contains(&site) {
            unique.push(site);
        }
    }
    unique
}

/// Whether `command` holds any destructive discard (no names seeded).
pub(super) fn has_discard(command: &[char]) -> bool {
    !find_discard_sites(command, &Names::new(), &Names::new()).is_empty()
}

/// [`has_discard`] on a `&str` (the Python `is_destructive_git_discard_command`).
#[cfg(test)]
pub(super) fn is_destructive_git_discard_command(command: &str) -> bool {
    has_discard(&super::text::chars(command))
}

#[cfg(test)]
mod tests {
    use super::super::text::chars;
    use super::*;

    #[test]
    fn restore_flags_follow_getopt() {
        for (region, expected) in [
            ("--staged ", false),
            ("-S ", false),
            ("-SW ", true),
            ("-s STASH ", true),
            ("-sSTASH ", true),
            ("--staged --worktree ", true),
            ("", true),
        ] {
            assert_eq!(
                restore_options_discard_worktree(&chars(region)),
                expected,
                "{region}"
            );
        }
    }

    #[test]
    fn clean_dry_runs_are_not_destructive() {
        assert!(!is_destructive_clean_segment(&chars(" -n -f .")));
        assert!(is_destructive_clean_segment(&chars(" -f -- -n")));
        assert!(!is_destructive_clean_segment(&chars(" --dry-run")));
    }
}
