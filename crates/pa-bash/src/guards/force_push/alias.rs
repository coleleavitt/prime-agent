//! git aliases defined on the command line itself (`git -c alias.p='push -f
//! origin main' p`) and git's own command table, which decides whether a
//! subcommand name is one git runs itself or one an alias (or an external
//! `git-<name>` program) can stand behind.

use super::budget::{Budget, Scan};
use super::push::{find_push_subcommand, GIT_GLOBAL_VALUE_LONG, GIT_GLOBAL_VALUE_SHORT};
use super::text::{command_name, is_space, strip_chars};
use super::words::{all_literal, invocation_tokens, literal_words, ShellWord};

/// git's own command table (`git --list-cmds=builtins,main`), calibrated to
/// Apple git 2.50.1 intersected with Homebrew git 2.55.0 (169 names), sorted.
///
/// git resolves a name in this table before any alias, so a name outside it is
/// a repository or user alias or an external `git-<name>` program, and either
/// can run a force push the command text does not show. The set is static on
/// purpose: the running git cannot be asked (a per-command `PATH` change could
/// make an older git run), so a command only a newer git knows is refused.
const GIT_COMMANDS: &[&str] = &[
    "add",
    "am",
    "annotate",
    "apply",
    "archive",
    "backfill",
    "bisect",
    "blame",
    "branch",
    "bugreport",
    "bundle",
    "cat-file",
    "check-attr",
    "check-ignore",
    "check-mailmap",
    "check-ref-format",
    "checkout",
    "checkout--worker",
    "checkout-index",
    "cherry",
    "cherry-pick",
    "clean",
    "clone",
    "column",
    "commit",
    "commit-graph",
    "commit-tree",
    "config",
    "count-objects",
    "credential",
    "credential-cache",
    "credential-cache--daemon",
    "credential-osxkeychain",
    "credential-store",
    "daemon",
    "describe",
    "diagnose",
    "diff",
    "diff-files",
    "diff-index",
    "diff-pairs",
    "diff-tree",
    "difftool",
    "difftool--helper",
    "fast-export",
    "fast-import",
    "fetch",
    "fetch-pack",
    "filter-branch",
    "fmt-merge-msg",
    "for-each-ref",
    "for-each-repo",
    "format-patch",
    "fsck",
    "fsck-objects",
    "fsmonitor--daemon",
    "gc",
    "get-tar-commit-id",
    "grep",
    "hash-object",
    "help",
    "hook",
    "http-backend",
    "http-fetch",
    "http-push",
    "imap-send",
    "index-pack",
    "init",
    "init-db",
    "interpret-trailers",
    "log",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "mailinfo",
    "mailsplit",
    "maintenance",
    "merge",
    "merge-base",
    "merge-file",
    "merge-index",
    "merge-octopus",
    "merge-one-file",
    "merge-ours",
    "merge-recursive",
    "merge-recursive-ours",
    "merge-recursive-theirs",
    "merge-resolve",
    "merge-subtree",
    "merge-tree",
    "mergetool",
    "mktag",
    "mktree",
    "multi-pack-index",
    "mv",
    "name-rev",
    "notes",
    "p4",
    "pack-objects",
    "pack-redundant",
    "pack-refs",
    "patch-id",
    "pickaxe",
    "prune",
    "prune-packed",
    "pull",
    "push",
    "quiltimport",
    "range-diff",
    "read-tree",
    "rebase",
    "receive-pack",
    "reflog",
    "refs",
    "remote",
    "remote-ext",
    "remote-fd",
    "remote-ftp",
    "remote-ftps",
    "remote-http",
    "remote-https",
    "repack",
    "replace",
    "replay",
    "request-pull",
    "rerere",
    "reset",
    "restore",
    "rev-list",
    "rev-parse",
    "revert",
    "rm",
    "send-email",
    "send-pack",
    "sh-i18n--envsubst",
    "shell",
    "shortlog",
    "show",
    "show-branch",
    "show-index",
    "show-ref",
    "sparse-checkout",
    "stage",
    "stash",
    "status",
    "stripspace",
    "submodule",
    "submodule--helper",
    "subtree",
    "switch",
    "symbolic-ref",
    "tag",
    "unpack-file",
    "unpack-objects",
    "update-index",
    "update-ref",
    "update-server-info",
    "upload-archive",
    "upload-archive--writer",
    "upload-pack",
    "var",
    "verify-commit",
    "verify-pack",
    "verify-tag",
    "version",
    "web--browse",
    "whatchanged",
    "worktree",
    "write-tree",
];

/// The git command words (matched case-folded).
pub(super) const GIT_COMMAND_NAMES: [&str; 2] = ["git", "git.exe"];

/// Characters a re-parsed payload can leave on the edge of a word when one
/// escaping layer is consumed (`git status\"` scans as `status`): trimmed
/// before the name check. The mangling only adds or drops quote characters,
/// so it cannot turn one command name into another.
pub(super) const WORD_EDGE_NOISE: &str = " \t\r\n\"'\\`;&|()<>";

/// An alias chain longer than this is refused.
const MAX_ALIAS_DEPTH: usize = 10;

/// Whether git runs `name` itself, whatever aliases exist.
pub(super) fn is_git_command(name: &str) -> bool {
    GIT_COMMANDS.binary_search(&name).is_ok()
}

/// An alias body the guard must not guess at: a shell (`!`) alias, or one
/// carrying substitution, quoting, or control syntax.
fn is_unresolvable_body(body: &str) -> bool {
    body.contains(|ch| "$`'\"\\;&|()<>#!\n".contains(ch))
}

/// The inline `alias.*` definitions of a `git ...` command line.
struct InlineAliases {
    /// Name to body; `None` when the body comes from the environment
    /// (`--config-env=alias.p=VAR`). A later definition wins.
    bodies: Vec<(String, Option<String>)>,
    /// Index of the subcommand word, `tokens.len()` when there is none.
    subcommand_index: usize,
}

impl InlineAliases {
    fn body(&self, name: &str) -> Option<&Option<String>> {
        self.bodies
            .iter()
            .rev()
            .find(|(alias, _)| alias == name)
            .map(|(_, body)| body)
    }
}

/// The inline aliases in the global-option region of `tokens` (`tokens[0]`
/// is the git word).
fn inline_aliases(tokens: &[String]) -> InlineAliases {
    let mut bodies = Vec::new();
    let n = tokens.len();
    let mut i = 1;
    while i < n {
        let token = tokens[i].as_str();
        if token == "--" {
            return InlineAliases {
                bodies,
                subcommand_index: n,
            };
        }
        if !token.starts_with('-') || token == "-" {
            return InlineAliases {
                bodies,
                subcommand_index: i,
            };
        }
        let mut value: Option<&str> = None;
        let mut from_environment = false;
        if token == "-c" || token == "--config-env" {
            if i + 1 >= n {
                return InlineAliases {
                    bodies,
                    subcommand_index: n,
                };
            }
            value = Some(&tokens[i + 1]);
            from_environment = token == "--config-env";
            i += 2;
        } else if let Some(rest) = token.strip_prefix("--config-env=") {
            value = Some(rest);
            from_environment = true;
            i += 1;
        } else if token.starts_with("-c") && token.chars().count() > 2 {
            value = Some(&token[2..]);
            i += 1;
        } else if GIT_GLOBAL_VALUE_SHORT.contains(&token) || GIT_GLOBAL_VALUE_LONG.contains(&token)
        {
            i += 2;
        } else {
            i += 1;
        }
        let Some(definition) = value.and_then(|value| value.strip_prefix("alias.")) else {
            continue;
        };
        if let Some((name, body)) = definition.split_once('=') {
            if !name.is_empty() {
                bodies.push((
                    name.to_string(),
                    (!from_environment).then(|| body.to_string()),
                ));
            }
        }
    }
    InlineAliases {
        bodies,
        subcommand_index: n,
    }
}

/// What one inline alias expansion step produced.
enum AliasStep {
    /// No inline alias applies to the invoked name.
    NotApplied,
    /// The argv git runs after rewriting the alias.
    Expanded(Vec<String>),
    /// The body cannot be expanded statically.
    Unresolvable,
}

/// Rewrite `git ... -c alias.X=<body> ... X ...` into the argv git runs.
fn expand_one(tokens: &[String], budget: &Budget) -> Scan<AliasStep> {
    let aliases = inline_aliases(tokens);
    if aliases.bodies.is_empty() || aliases.subcommand_index >= tokens.len() {
        return Ok(AliasStep::NotApplied);
    }
    let index = aliases.subcommand_index;
    let Some(body) = aliases.body(&tokens[index]) else {
        return Ok(AliasStep::NotApplied);
    };
    let Some(body) = body.as_deref().filter(|body| !is_unresolvable_body(body)) else {
        return Ok(AliasStep::Unresolvable);
    };
    budget.charge()?; // an alias body is re-scanned as argv
    let (words, well_formed) = literal_words(body.trim_matches(is_space));
    if !well_formed || words.is_empty() || !all_literal(&words) {
        return Ok(AliasStep::Unresolvable);
    }
    let mut expanded = tokens[..index].to_vec();
    expanded.extend(words.into_iter().flatten());
    expanded.extend_from_slice(&tokens[index + 1..]);
    Ok(AliasStep::Expanded(expanded))
}

/// Expand inline aliases until the argv stops changing; `None` for a body
/// the guard cannot expand or a chain longer than [`MAX_ALIAS_DEPTH`].
fn expand_chain(tokens: &[String], budget: &Budget) -> Scan<Option<Vec<String>>> {
    let mut current = tokens.to_vec();
    for _ in 0..MAX_ALIAS_DEPTH {
        match expand_one(&current, budget)? {
            AliasStep::Unresolvable => return Ok(None),
            AliasStep::NotApplied => return Ok(Some(current)),
            AliasStep::Expanded(expanded) if expanded == current => return Ok(Some(current)),
            AliasStep::Expanded(expanded) => current = expanded,
        }
    }
    Ok(None)
}

/// The subcommand a `git ...` command line runs.
enum Subcommand {
    /// The line invokes no subcommand.
    Absent,
    /// An inline alias the guard cannot expand stands in the way.
    Unresolvable,
    Named(String),
}

fn effective_subcommand(tokens: &[String], budget: &Budget) -> Scan<Subcommand> {
    let index = inline_aliases(tokens).subcommand_index;
    if index >= tokens.len() {
        return Ok(Subcommand::Absent);
    }
    if is_git_command(&tokens[index]) {
        return Ok(Subcommand::Named(tokens[index].clone()));
    }
    let Some(expanded) = expand_chain(tokens, budget)? else {
        return Ok(Subcommand::Unresolvable);
    };
    let index = inline_aliases(&expanded).subcommand_index;
    Ok(expanded
        .get(index)
        .map_or(Subcommand::Absent, |name| Subcommand::Named(name.clone())))
}

/// Resolve inline aliases for the invoked subcommand: the tokens unchanged
/// when no alias applies, when git runs the name itself, or when the
/// expansion holds no `push`; the expansion when it carries a push; `None`
/// when the body cannot be expanded.
pub(super) fn expand_inline_git_aliases(
    tokens: &[String],
    budget: &Budget,
) -> Scan<Option<Vec<String>>> {
    let index = inline_aliases(tokens).subcommand_index;
    if tokens.get(index).is_some_and(|name| is_git_command(name)) {
        return Ok(Some(tokens.to_vec()));
    }
    let Some(expanded) = expand_chain(tokens, budget)? else {
        return Ok(None);
    };
    if expanded == tokens || find_push_subcommand(&expanded).0.is_none() {
        return Ok(Some(tokens.to_vec()));
    }
    Ok(Some(expanded))
}

/// The first git subcommand outside git's own command table (a repository
/// alias or an external `git-<name>` program can stand behind it), after
/// following inline aliases.
pub(super) fn unresolvable_git_subcommand(
    words: &[ShellWord],
    budget: &Budget,
) -> Scan<Option<String>> {
    for (index, word) in words.iter().enumerate() {
        if !GIT_COMMAND_NAMES.contains(&command_name(&word.value).as_str()) {
            continue;
        }
        let tokens = invocation_tokens(words, index);
        let Subcommand::Named(subcommand) = effective_subcommand(&tokens, budget)? else {
            continue;
        };
        let name = strip_chars(&subcommand, WORD_EDGE_NOISE);
        if name.is_empty() || is_git_command(name) {
            continue;
        }
        return Ok(Some(name.to_string()));
    }
    Ok(None)
}
