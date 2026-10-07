//! `git ... push` invocations: finding them in the word scan, the context
//! they run in, and the push arguments that matter to the guard.

use super::alias::{expand_inline_git_aliases, GIT_COMMAND_NAMES};
use super::budget::{Budget, Scan};
use super::command_words::{is_unmodeled_wrapper, COMMAND_WRAPPERS, UNMODELED_WRAPPERS};
use super::text::{
    command_name, has_glob_or_substitution, starts_with_assignment, starts_with_git_assignment,
};
use super::words::{invocation_tokens, ShellWord};

/// git global options that take the next token as their value.
pub(super) const GIT_GLOBAL_VALUE_SHORT: [&str; 2] = ["-c", "-C"];
pub(super) const GIT_GLOBAL_VALUE_LONG: [&str; 6] = [
    "--git-dir",
    "--git-common-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
    "--config-env",
];

const XARGS_COMMAND_NAMES: [&str; 2] = ["xargs", "xargs.exe"];
const ENV_COMMAND_NAMES: [&str; 2] = ["env", "env.exe"];

/// git's own push long options (`git push -h`, git 2.55), each also in its
/// `--no-` form, resolved by unique prefix the way parse-options does.
const PUSH_LONG_OPTIONS: [&str; 28] = [
    "verbose",
    "quiet",
    "repo",
    "all",
    "branches",
    "mirror",
    "delete",
    "tags",
    "dry-run",
    "porcelain",
    "force",
    "force-with-lease",
    "force-if-includes",
    "recurse-submodules",
    "thin",
    "receive-pack",
    "exec",
    "set-upstream",
    "progress",
    "prune",
    "verify",
    "no-verify",
    "follow-tags",
    "signed",
    "atomic",
    "push-option",
    "ipv4",
    "ipv6",
];

/// Push long options taking a separate value word.
const PUSH_VALUE_LONG: [&str; 4] = ["--receive-pack", "--exec", "--repo", "--push-option"];

/// Every spelling `git push` accepts, de-duplicated (`--no-verify` is both a
/// declared option and the negation of `verify`).
fn push_long_spellings() -> Vec<String> {
    let mut spellings: Vec<String> = Vec::new();
    for option in PUSH_LONG_OPTIONS {
        for spelling in [option.to_string(), format!("no-{option}")] {
            if !spellings.contains(&spelling) {
                spellings.push(spelling);
            }
        }
    }
    spellings
}

/// How a `--word` token reads as a push long option.
#[derive(Debug, PartialEq, Eq)]
enum LongOption {
    /// An exact name or a unique prefix of one.
    Named(String),
    /// A prefix of more than one option (git rejects it).
    Ambiguous,
    /// None of git's own push options.
    Unknown,
}

fn push_long_option(token: &str) -> LongOption {
    let name = token[2..].split('=').next().unwrap_or_default();
    let spellings = push_long_spellings();
    if spellings.iter().any(|spelling| spelling == name) {
        return LongOption::Named(name.to_string());
    }
    let mut matches = spellings
        .iter()
        .filter(|spelling| spelling.starts_with(name));
    match (matches.next(), matches.next()) {
        (Some(_), Some(_)) => LongOption::Ambiguous,
        (Some(only), None) => LongOption::Named(only.clone()),
        (None, _) => LongOption::Unknown,
    }
}

/// Index of the `push` subcommand in `tokens` (`tokens[0]` is the git word),
/// plus whether global options relocate the repository.
pub(super) fn find_push_subcommand(tokens: &[String]) -> (Option<usize>, bool) {
    let n = tokens.len();
    let mut relocated = false;
    let mut i = 1;
    while i < n {
        let token = tokens[i].as_str();
        if token == "--" {
            return (None, relocated);
        }
        if !token.starts_with('-') || token == "-" {
            return ((token == "push").then_some(i), relocated);
        }
        if token == "-C" {
            relocated = true;
            i += 2;
            continue;
        }
        if token.starts_with("-C") && token.chars().count() > 2 {
            relocated = true;
            i += 1;
            continue;
        }
        if GIT_GLOBAL_VALUE_SHORT.contains(&token) {
            i += 2;
            continue;
        }
        let attached = GIT_GLOBAL_VALUE_LONG
            .iter()
            .any(|option| token.starts_with(&format!("{option}=")));
        if GIT_GLOBAL_VALUE_LONG.contains(&token) || attached {
            relocated = true;
            i += if attached { 1 } else { 2 };
            continue;
        }
        if token == "--exec-path" || token == "-h" || token == "--help" {
            return (None, relocated);
        }
        i += 1;
    }
    (None, relocated)
}

/// One `git ... push` invocation found by the word scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PushRun {
    /// Index of the git word in the scan.
    pub git_index: usize,
    /// Index of the push token within `tokens`.
    pub push_index: usize,
    /// argv values from the git word to the run's end.
    pub tokens: Vec<String>,
    /// `-C`/`--git-dir`-style relocation or a `GIT_DIR=...` prefix.
    pub relocated: bool,
    /// xargs feeds refspecs the guard cannot see.
    pub xargs_fed: bool,
    /// An inline `alias.X` hides this run.
    pub unresolvable_alias: bool,
}

/// The semantics of one git push invocation that matter to the guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PushArgs {
    pub force: bool,
    pub dry_run: bool,
    /// `--all` / `--mirror`: every branch is a target.
    pub wildcard: bool,
    pub refspecs: Vec<String>,
    /// The argv word holding a variable, glob or substitution.
    pub unresolvable: Option<String>,
}

/// A refspec that is entirely git's `@{...}` syntax (`@{u}`, `@{-1}`).
fn is_static_at_brace(token: &str) -> bool {
    token
        .strip_prefix("@{")
        .and_then(|rest| rest.strip_suffix('}'))
        .is_some_and(|body| {
            body.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "_./-".contains(ch))
        })
}

/// Parse the `git push` argv after the subcommand word.
pub(super) fn parse_push_args(tokens: &[String], push_index: usize) -> PushArgs {
    let mut force = false;
    let mut dry_run = false;
    let mut all_refs = false;
    let mut mirror = false;
    let mut unresolvable: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut options_done = false;
    let n = tokens.len();
    let mut i = push_index + 1;
    while i < n {
        let mut token = tokens[i].clone();
        if unresolvable.is_none() && has_glob_or_substitution(&token) && !is_static_at_brace(&token)
        {
            // An expansion can become `-f`, a protected `+`-refspec, or the
            // remote; exactly `@{...}` is git's own syntax.
            unresolvable = Some(token.clone());
        }
        if options_done {
            positionals.push(token);
            i += 1;
            continue;
        }
        if token == "--" {
            options_done = true;
            i += 1;
            continue;
        }
        if token.starts_with("--") {
            match push_long_option(&token) {
                LongOption::Ambiguous => unresolvable = Some(token.clone()),
                LongOption::Named(resolved) => {
                    let name_len = token[2..].split('=').next().unwrap_or_default().len();
                    let attached = token[2 + name_len..].to_string();
                    let name = if resolved == "branches" {
                        "all"
                    } else {
                        resolved.as_str()
                    };
                    token = format!("--{name}{attached}");
                }
                LongOption::Unknown => {}
            }
            // The `--no-` form of every boolean; the last one wins.
            match token.as_str() {
                "--force" => force = true,
                "--no-force" => force = false,
                "--dry-run" => dry_run = true,
                "--no-dry-run" => dry_run = false,
                "--all" => all_refs = true,
                "--no-all" => all_refs = false,
                "--mirror" => mirror = true,
                "--no-mirror" => mirror = false,
                value
                    if value.starts_with("--force-with-lease")
                        || value.starts_with("--force-if-includes") => {}
                value if PUSH_VALUE_LONG.contains(&value) => i += 1,
                _ => {}
            }
            i += 1;
            continue;
        }
        if token.starts_with('-') && token != "-" {
            let cluster: Vec<char> = token.chars().skip(1).collect();
            let mut consumes_value = false;
            for (position, ch) in cluster.iter().enumerate() {
                match ch {
                    'f' => force = true,
                    'n' => dry_run = true,
                    'o' => {
                        // git reads the rest of the cluster as the value.
                        consumes_value = position == cluster.len() - 1;
                        break;
                    }
                    _ => {}
                }
            }
            i += if consumes_value { 2 } else { 1 };
            continue;
        }
        positionals.push(token);
        i += 1;
    }
    // git always reads the first positional as the repository, so the
    // refspecs are the positionals after it (`--repo` does not change that).
    let refspecs = positionals.into_iter().skip(1).collect();
    PushArgs {
        force: force || mirror,
        dry_run,
        wildcard: all_refs || mirror,
        refspecs,
        unresolvable,
    }
}

/// True when the invocation carries force and is not a dry run. A word the
/// scan cannot resolve counts as force (it can expand into `-f`, a
/// `+`-refspec, or `--no-dry-run`), even alongside `--dry-run`.
pub(super) fn is_guarded_push(args: &PushArgs) -> bool {
    if args.unresolvable.is_some() {
        return true;
    }
    if args.dry_run {
        return false;
    }
    args.force || args.refspecs.iter().any(|spec| spec.starts_with('+'))
}

/// Whether one scanned run needs the violation check.
pub(super) fn run_is_guarded(run: &PushRun) -> bool {
    run.unresolvable_alias || is_guarded_push(&parse_push_args(&run.tokens, run.push_index))
}

/// Whether the git invocation at `git_index` is relocated or xargs-fed,
/// from the words before it in the same command run (env assignments,
/// wrappers and xargs may precede it; a real command word stops the walk).
fn invocation_context(words: &[ShellWord], git_index: usize) -> (bool, bool) {
    let mut relocated = false;
    let mut xargs_fed = false;
    let mut command_start = git_index;
    while command_start > 0 && !words[command_start].starts_command {
        command_start -= 1;
    }
    if words[command_start..git_index]
        .iter()
        .any(|word| ENV_COMMAND_NAMES.contains(&command_name(&word.value).as_str()))
    {
        // `env -C DIR git push -f` can move the invocation.
        relocated = true;
    }
    for previous in words[..git_index].iter().rev() {
        let value = previous.value.as_str();
        let name = command_name(value);
        let is_wrapper = COMMAND_WRAPPERS.contains(&name.as_str());
        let is_xargs = XARGS_COMMAND_NAMES.contains(&name.as_str());
        if previous.starts_command && !(starts_with_assignment(value) || is_xargs || is_wrapper) {
            break;
        }
        if previous.contained {
            continue;
        }
        if is_xargs {
            xargs_fed = true;
        } else if starts_with_git_assignment(value) {
            relocated = true;
        } else if starts_with_assignment(value) {
            // A benign assignment applies only to this invocation.
        } else if is_wrapper {
            if UNMODELED_WRAPPERS.contains(&name.as_str()) {
                relocated = true;
            }
        } else if is_unmodeled_wrapper(value) {
            relocated = true;
        } else {
            break;
        }
    }
    (relocated, xargs_fed)
}

/// Every `git ... push` invocation, as argv token runs. Quoted command
/// names, subcommands and flags fold into the word values, so they scan like
/// their unquoted forms.
pub(super) fn find_git_push_runs(words: &[ShellWord], budget: &Budget) -> Scan<Vec<PushRun>> {
    let mut runs = Vec::new();
    for (index, word) in words.iter().enumerate() {
        if !GIT_COMMAND_NAMES.contains(&command_name(&word.value).as_str()) {
            continue;
        }
        let tokens = invocation_tokens(words, index);
        let (prefix_relocated, xargs_fed) = invocation_context(words, index);
        let Some(expanded) = expand_inline_git_aliases(&tokens, budget)? else {
            runs.push(PushRun {
                git_index: index,
                push_index: 0,
                tokens,
                relocated: prefix_relocated,
                xargs_fed,
                unresolvable_alias: true,
            });
            continue;
        };
        let (push_index, global_relocated) = find_push_subcommand(&expanded);
        let Some(push_index) = push_index else {
            continue;
        };
        runs.push(PushRun {
            git_index: index,
            push_index,
            tokens: expanded,
            relocated: global_relocated || prefix_relocated,
            xargs_fed,
            unresolvable_alias: false,
        });
    }
    Ok(runs)
}
