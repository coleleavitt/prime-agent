//! Force-push rule: a `git push` that can force (`-f`, `--force`, `--mirror`,
//! a `+refspec`, or an argument only known at run time that can become one)
//! is refused while its target is protected: `main`/`master`, `@{u}`, every
//! branch (`--all`, `--mirror`, `:`), or, without a refspec, the current
//! upstream (probed with a bounded `git rev-parse` in the directory the
//! push runs in).

use std::collections::HashMap;
use std::path::PathBuf;

use super::git::{directories, git_call, probe, GitCall, UPSTREAM_LIMITS};
use super::{opaque, Check, Rule};
use crate::model::{Arg, Invocation, Via};
use crate::verdict::GuardKind;

pub(crate) struct ForcePush;

const LATE_BYPASS_WARNING: &str = "prime-agent bash: PI_BASH_ALLOW_FORCE_PUSH appeared after kernel start and is ignored; the force-push guard only honors it when the kernel is started with it set.";

impl Rule for ForcePush {
    const GUARD: GuardKind = GuardKind::ForcePush;
    const LATE_BYPASS_WARNING: Option<&'static str> = Some(LATE_BYPASS_WARNING);

    fn judge(check: &Check<'_>) -> Option<String> {
        let mirror_config = check.model.invocations.iter().any(sets_mirror_config);
        let mut probes = Probes::default();
        for invocation in &check.model.invocations {
            if let Some(refusal) = judge_invocation(invocation, check, mirror_config, &mut probes) {
                return Some(refusal.message(invocation));
            }
        }
        let (node, evidence) = opaque::evidenced(check.model, &opaque::ANY, force_push_evidence)?;
        Some(refusal(&opaque::reason(node, &evidence)))
    }
}

/// Upstream probe results by (command, directory).
type Probes = HashMap<(String, PathBuf), Result<Option<(String, String)>, ()>>;

/// Why a push was refused.
enum Refusal {
    /// A reason inside the shared force-push message.
    Reason(String),
    /// A message with its own lead and advice.
    Standalone(String, &'static str),
}

impl Refusal {
    fn message(self, invocation: &Invocation) -> String {
        let place = invocation.place();
        let detail = if place.is_empty() {
            String::new()
        } else {
            format!(" ({place}: `{}`)", invocation.shown())
        };
        match self {
            Refusal::Reason(reason) => refusal(&format!("{reason}{detail}")),
            Refusal::Standalone(lead, advice) => standalone(&format!("{lead}{detail}"), advice),
        }
    }
}

fn refusal(reason: &str) -> String {
    format!(
        "Refusing to run this force-push command: {reason}.\n\nForce-pushes rewrite remote history; a force-push to main/master or the current upstream can discard other people's work in one step.\n\nUse --force-with-lease instead: it refuses to overwrite unless the remote ref still matches what you have.\n\nTo force-push anyway, retry with bash(command, allow_force_push=True), or start the kernel with PI_BASH_ALLOW_FORCE_PUSH=1."
    )
}

fn standalone(lead: &str, advice: &str) -> String {
    format!(
        "Refusing to run this force-push command: {lead}\n\n{advice} bash(command, allow_force_push=True), or start the kernel with PI_BASH_ALLOW_FORCE_PUSH=1."
    )
}

const PROTECTED_BRANCHES: [&str; 2] = ["main", "master"];

/// `git config remote.X.mirror ...` / `remote.X.push`: a later push in the
/// same command can be forced by configuration.
fn sets_mirror_config(invocation: &Invocation) -> bool {
    let Some(call) = git_call(invocation) else {
        return false;
    };
    call.subcommand_is("config")
        && call
            .args
            .iter()
            .any(|arg| arg.known().is_some_and(is_mirror_or_push_key))
}

fn is_mirror_or_push_key(text: &str) -> bool {
    let key = text.split('=').next().unwrap_or(text).to_ascii_lowercase();
    key.starts_with("remote.") && matches!(key.rsplit('.').next(), Some("mirror" | "push"))
}

fn judge_invocation(
    invocation: &Invocation,
    check: &Check<'_>,
    mirror_config: bool,
    probes: &mut Probes,
) -> Option<Refusal> {
    let call = git_call(invocation)?;
    let (subcommand, args) = match expand_alias(&call, check) {
        Ok(Some(expanded)) => expanded,
        Ok(None) => return None,
        Err(refusal) => return Some(refusal),
    };
    if subcommand != "push" {
        return None;
    }
    if let Some(refusal) = config_refusal(&call, invocation, mirror_config) {
        return Some(refusal);
    }
    let push = PushArgs::parse(&args);
    if push.dry_run {
        return None;
    }
    if let Some(option) = &push.ambiguous {
        return Some(Refusal::Reason(format!(
            "the option \"{option}\" abbreviates more than one push option, one of them a force"
        )));
    }
    let xargs = invocation
        .layers
        .iter()
        .any(|layer| layer.name == "xargs" || layer.name == "parallel");
    let flag_force = push.force || push.every.mirror;
    if push.every.mirror || (push.every.all && flag_force) {
        return Some(Refusal::Reason(
            "a force flag with --all/--mirror rewrites every branch, including main/master and the current upstream".to_string(),
        ));
    }
    if xargs && (flag_force || push.refspecs.iter().any(|arg| arg.may_start_with('+'))) {
        return Some(Refusal::Reason(
            "xargs feeds it refspecs from stdin the guard cannot see".to_string(),
        ));
    }
    if let Some(word) = push.unresolved.iter().find(|word| shows_force(word)) {
        // An argument that starts with an expansion can become `-f` or a
        // `+refspec` when the text it comes from carries one; a word with a
        // fixed start (`offer/$n`) never can.
        {
            return Some(Refusal::Reason(format!(
                "the push argument \"{}\" is only known at run time: the shell may expand it into a force flag or into a refspec naming a protected branch before git reads argv",
                word.shown()
            )));
        }
    }
    let mut any_forced = flag_force;
    for refspec in &push.refspecs {
        let forced = flag_force
            || matches!(refspec, Arg::Known(text) if text.starts_with('+'))
            || (!matches!(refspec, Arg::Known(_))
                && refspec.may_start_with('+')
                && shows_force(refspec));
        if !forced {
            continue;
        }
        any_forced = true;
        let text = match refspec {
            Arg::Known(text) => text,
            Arg::Unknown(_) if names_a_safe_branch(refspec) => continue,
            Arg::Pattern(_) | Arg::Unknown(_) => {
                return Some(Refusal::Reason(format!(
                    "the push target \"{}\" cannot be verified statically (glob, substitution, or variable)",
                    refspec.shown()
                )))
            }
        };
        if let Some(refusal) = judge_forced_refspec(text, &call, check, probes) {
            return Some(refusal);
        }
    }
    if push.refspecs.is_empty() && any_forced {
        return match current_branch(&call, check, probes) {
            Ok(None) => None,
            Ok(Some((upstream, current))) if upstream.is_empty() => Some(Refusal::Reason(format!(
                "without a refspec, and with no upstream on the current branch \"{current}\", the push target comes from push.default, remote.<name>.push, or remote.<name>.mirror configuration the guard cannot read"
            ))),
            Ok(Some((upstream, _))) => Some(Refusal::Reason(format!(
                "without a refspec it would force-push the current branch onto its upstream \"{upstream}\""
            ))),
            Err(refusal) => Some(refusal),
        };
    }
    None
}

/// Whether a value only known at run time can carry a force: the text it
/// comes from shows a force flag or a `+refspec`.
fn shows_force(word: &Arg) -> bool {
    match word {
        Arg::Known(_) => false,
        Arg::Pattern(_) | Arg::Unknown(_) => {
            let evidence = word.evidence();
            evidence
                .split(|ch: char| ch.is_whitespace() || "'\"`;|&()=".contains(ch))
                .any(|token| {
                    matches!(token, "--force" | "--mirror")
                        || (token.starts_with('-')
                            && !token.starts_with("--")
                            && token.len() > 1
                            && token[1..].chars().all(|ch| ch.is_ascii_alphabetic())
                            && token.contains('f'))
                        || (token.len() > 1 && token.starts_with('+'))
                })
        }
    }
}

/// A refspec only known at run time whose fixed start and source cannot
/// name a protected branch (`offer/$n` with `n` from `a b`).
fn names_a_safe_branch(refspec: &Arg) -> bool {
    let prefix = refspec.fixed_prefix();
    if prefix.is_empty() || prefix.starts_with('+') {
        return false;
    }
    let evidence = refspec.evidence();
    let protected = PROTECTED_BRANCHES
        .iter()
        .any(|branch| evidence.contains(branch))
        || evidence.contains(':')
        || evidence.contains("@{")
        || evidence.contains("HEAD");
    let branch = prefix
        .strip_prefix("refs/heads/")
        .or_else(|| prefix.strip_prefix("heads/"))
        .unwrap_or(prefix);
    !protected
        && !PROTECTED_BRANCHES
            .iter()
            .any(|name| name.starts_with(branch))
        && branch != "@"
}

fn mirror_config_refusal() -> Refusal {
    Refusal::Reason("a remote.<name>.mirror or remote.<name>.push setting in this command can turn the push into a forced one the guard cannot verify (a mirror remote force-updates every ref; a configured push refspec can carry a +)".to_string())
}

const UPSTREAM_SCRIPT: &str = "cur=$({git} rev-parse --abbrev-ref HEAD 2>/dev/null) || exit 1\nup=$({git} rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null) || up=\nprintf '%s\\n%s\\n' \"$up\" \"$cur\"";

/// A configuration this push runs with that can force it: an inline `-c`,
/// `--config-env` or `GIT_CONFIG_*` setting of remote.<name>.push/mirror (or
/// one the guard cannot read), or a write of one earlier in the command.
fn config_refusal(
    call: &GitCall<'_>,
    invocation: &Invocation,
    mirror_config: bool,
) -> Option<Refusal> {
    for value in call.config_values() {
        match value {
            None => {
                return Some(Refusal::Reason(format!(
                    "its inline config operand \"{}\" is only known at run time, so the configuration it applies -- which can arm a force push through remote.<name>.push or remote.<name>.mirror -- cannot be checked",
                    call.configs.iter().find(|arg| arg.known().is_none()).map(|arg| arg.shown()).unwrap_or_default()
                )))
            }
            Some(text) if is_mirror_or_push_key(&text) => return Some(mirror_config_refusal()),
            Some(_) => {}
        }
    }
    let config_env = call.global.windows(2).any(|pair| {
        pair[0].known() == Some("--config-env") && pair[1].known().is_none_or(is_mirror_or_push_key)
    }) || call.global.iter().any(|arg| {
        arg.known()
            .and_then(|text| text.strip_prefix("--config-env="))
            .is_some_and(is_mirror_or_push_key)
    });
    if config_env {
        return Some(mirror_config_refusal());
    }
    let env_config = invocation.env.iter().any(|(name, value)| {
        name == "GIT_CONFIG_PARAMETERS"
            || (name.starts_with("GIT_CONFIG_KEY_")
                && value.known().is_none_or(is_mirror_or_push_key))
    });
    if mirror_config || env_config {
        return Some(mirror_config_refusal());
    }
    None
}

/// The refusal for a forced refspec `text`, if its destination is protected.
fn judge_forced_refspec(
    text: &str,
    call: &GitCall<'_>,
    check: &Check<'_>,
    probes: &mut Probes,
) -> Option<Refusal> {
    let body = text.strip_prefix('+').unwrap_or(text);
    if body == ":" {
        return Some(Refusal::Reason(
            "the refspec \":\" force-pushes every matching branch, including main/master"
                .to_string(),
        ));
    }
    let destination = match body.split_once(':') {
        Some((source, "")) => source,
        Some((_, destination)) => destination,
        None => body,
    };
    if destination.is_empty() {
        return None;
    }
    if destination.contains("@{") {
        return Some(Refusal::Reason(format!(
            "the refspec \"{text}\" names the current upstream"
        )));
    }
    if destination.contains(['*', '?', '[']) {
        return Some(Refusal::Reason(format!(
            "the push target \"{destination}\" cannot be verified statically (glob, substitution, or variable)"
        )));
    }
    let branch = destination
        .strip_prefix("refs/heads/")
        .or_else(|| destination.strip_prefix("heads/"))
        .unwrap_or(destination);
    if PROTECTED_BRANCHES.contains(&branch) {
        return Some(Refusal::Reason(format!("it would force-push \"{branch}\"")));
    }
    if matches!(branch, "HEAD" | "@") {
        match current_branch(call, check, probes) {
            Ok(Some((_, current))) if PROTECTED_BRANCHES.contains(&current.as_str()) => {
                return Some(Refusal::Reason(format!(
                    "HEAD names the current branch \"{current}\""
                )))
            }
            Ok(_) => {}
            Err(refusal) => return Some(refusal),
        }
    }

    None
}

/// `(upstream, current branch)` where the push runs; `None` outside a
/// repository (git fails on its own).
fn current_branch(
    call: &GitCall<'_>,
    check: &Check<'_>,
    probes: &mut Probes,
) -> Result<Option<(String, String)>, Refusal> {
    let invocation = call.invocation;
    if let Some(Via::Remote { host }) = invocation
        .context
        .iter()
        .find(|via| matches!(via, Via::Remote { .. }))
    {
        return Err(Refusal::Reason(format!(
            "it runs on the remote host {host}, where the guard cannot read the branch it would rewrite"
        )));
    }
    let dirs = directories(invocation).map_err(|relocation| {
        Refusal::Standalone(
            format!("it changes directory (or relocates the repository) first (`{relocation}`), and the branch it would rewrite cannot be determined safely."),
            "Run it as its own command from the target directory, or retry with",
        )
    })?;
    if !call.replayable() {
        return Err(Refusal::Standalone(
            "its git options or environment are only known at run time (it changes directory or relocates the repository), and the branch it would rewrite cannot be determined safely.".to_string(),
            "Run it as its own command from the target directory, or retry with",
        ));
    }
    let command = call.probe_command(UPSTREAM_SCRIPT, check.script.prefix);
    let mut found = None;
    for dir in dirs {
        let key = (command.clone(), dir.clone());
        let result = probes
            .entry(key)
            .or_insert_with(|| {
                probe(check.context, &command, dir, UPSTREAM_LIMITS).map(|output| {
                    output.and_then(|(text, _)| {
                        let mut lines = text.lines();
                        let upstream = lines.next().unwrap_or_default().to_string();
                        let current = lines.next().unwrap_or_default().to_string();
                        (!current.is_empty()).then_some((upstream, current))
                    })
                })
            })
            .clone();
        match result {
            Err(()) => return Err(Refusal::Standalone(
                "resolving the branch it would rewrite timed out (a `git rev-parse` probe the guard runs before spawning anything), so the target cannot be determined safely.".to_string(),
                "Retry the command, or retry with",
            )),
            Ok(Some(branch)) => {
                if found.is_none() || PROTECTED_BRANCHES.contains(&branch.1.as_str()) {
                    found = Some(branch);
                }
            }
            Ok(None) => {}
        }
    }
    Ok(found)
}

/// git's own command table (`git --list-cmds=builtins,main`, 169 names): git
/// runs these itself before any alias, so only a name outside it can stand for
/// an alias.
const GIT_COMMANDS: [&str; 169] = [
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

/// The subcommand and arguments after alias expansion: `-c alias.NAME=BODY`
/// on the command line, then the alias git's configuration defines (read
/// with a `git config --get alias.NAME` probe where the command runs).
fn expand_alias(
    call: &GitCall<'_>,
    check: &Check<'_>,
) -> Result<Option<(String, Vec<Arg>)>, Refusal> {
    let mut args: Vec<Arg> = call.args.to_vec();
    let mut subcommand = match call.subcommand {
        None => return Ok(None),
        Some(Arg::Known(text)) => text.clone(),
        Some(other) => {
            let evidence = other.evidence();
            if crate::model::evidence::find_word(&evidence, "push").is_some() {
                return Err(Refusal::Reason(format!(
                    "its subcommand \"{}\" is only known at run time, and the text it comes from mentions push",
                    other.shown()
                )));
            }
            return Ok(None);
        }
    };
    let aliases: Vec<(String, String)> = call
        .config_values()
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let (key, body) = value.split_once('=')?;
            let name = key.strip_prefix("alias.")?;
            Some((name.to_string(), body.to_string()))
        })
        .collect();
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..10 {
        if seen.contains(&subcommand) {
            // git stops at an alias loop ("alias loop detected") and runs nothing.
            return Ok(None);
        }
        seen.push(subcommand.clone());
        if GIT_COMMANDS.contains(&subcommand.as_str()) {
            return Ok(Some((subcommand, args)));
        }
        let inline = aliases
            .iter()
            .rev()
            .find(|(name, _)| *name == subcommand)
            .map(|(_, body)| body.clone());
        let Some(body) = inline.map_or_else(
            || configured_alias(call, check, &subcommand),
            |body| Ok(Some(body)),
        )?
        else {
            return Ok(Some((subcommand, args)));
        };
        let body = body.as_str();
        let mentions_push = crate::model::evidence::find_word(body, "push").is_some();
        if body.starts_with('!')
            || body.contains(['$', '`', '\'', '"', ';', '&', '|', '(', ')', '<', '>'])
        {
            if mentions_push {
                return Err(Refusal::Standalone(
                    format!("it defines a git alias (`-c alias.{subcommand}=...`) that runs `{body}`, and the argv that alias expands to cannot be resolved safely."),
                    "Run the push directly with the aliased name spelled out, or retry with",
                ));
            }
            return Ok(None);
        }
        let mut words = body
            .split_whitespace()
            .map(|word| Arg::Known(word.to_string()));
        let Some(Arg::Known(first)) = words.next() else {
            return Ok(None);
        };
        let mut expanded: Vec<Arg> = words.collect();
        expanded.extend(args);
        args = expanded;
        subcommand = first;
    }
    Err(Refusal::Standalone(
        format!("it defines a chain of git aliases (`-c alias.{subcommand}=...`) too long to follow, and the argv it expands to cannot be resolved safely."),
        "Run the push directly with the aliased name spelled out, or retry with",
    ))
}

/// The body of the alias `name` in git's configuration where `call` runs,
/// if any. Outside a repository the global and system aliases still apply,
/// and the probe reads those too.
fn configured_alias(
    call: &GitCall<'_>,
    check: &Check<'_>,
    name: &str,
) -> Result<Option<String>, Refusal> {
    let unreadable = |why: &str| {
        Refusal::Standalone(
            format!("its subcommand \"{name}\" is not one git runs itself, so a git alias may stand behind it, and {why}."),
            "Spell out the real subcommand, or retry with",
        )
    };
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Ok(None);
    }
    let dirs = directories(call.invocation).map_err(|relocation| {
        unreadable(&format!(
            "it runs in a directory the guard cannot name (`{relocation}`)"
        ))
    })?;
    if !call.replayable() {
        return Err(unreadable(
            "its git options or environment are only known at run time",
        ));
    }
    let command = call.probe_command(
        &format!("{{git}} config --get alias.{name}"),
        check.script.prefix,
    );
    for dir in dirs {
        match probe(check.context, &command, dir, UPSTREAM_LIMITS) {
            Ok(Some((body, _))) if !body.trim().is_empty() => {
                return Ok(Some(body.trim().to_string()))
            }
            Ok(_) => {}
            Err(()) => return Err(unreadable("reading the alias timed out")),
        }
    }
    Ok(None)
}

/// `--all`/`--branches` and `--mirror`: the push names every branch.
#[derive(Debug, Default)]
struct Every {
    all: bool,
    mirror: bool,
}

/// `git push` argv, read the way git's option parser reads it.
#[derive(Debug, Default)]
struct PushArgs {
    force: bool,
    dry_run: bool,
    every: Every,
    refspecs: Vec<Arg>,
    /// Arguments only known at run time that may be options.
    unresolved: Vec<Arg>,
    ambiguous: Option<String>,
}

const LONG_OPTIONS: [&str; 28] = [
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
const VALUE_LONG_OPTIONS: [&str; 4] = ["repo", "receive-pack", "exec", "push-option"];

impl PushArgs {
    fn parse(args: &[Arg]) -> Self {
        let mut out = Self::default();
        let mut positionals: Vec<Arg> = Vec::new();
        let mut options_done = false;
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            index += 1;
            let text = match arg {
                Arg::Known(text) if !options_done => text.as_str(),
                Arg::Known(_) | Arg::Pattern(_) => {
                    positionals.push(arg.clone());
                    continue;
                }
                Arg::Unknown(_) => {
                    if !options_done && arg.may_start_with('-') {
                        out.unresolved.push(arg.clone());
                    }
                    positionals.push(arg.clone());
                    continue;
                }
            };
            if text == "--" {
                options_done = true;
                continue;
            }
            if let Some(long) = text.strip_prefix("--") {
                let (name, glued) = match long.split_once('=') {
                    Some((name, _)) => (name, true),
                    None => (long, false),
                };
                let (negated, bare) = match name.strip_prefix("no-") {
                    Some(bare) if name != "no-verify" => (true, bare),
                    Some(_) | None => (false, name),
                };
                let resolved = resolve_long(bare);
                match resolved {
                    Long::Exact(option) | Long::Unique(option) => {
                        match option {
                            "force" => out.force = !negated,
                            "dry-run" => out.dry_run = !negated,
                            "all" | "branches" => out.every.all = !negated,
                            "mirror" => out.every.mirror = !negated,
                            _ => {}
                        }
                        if VALUE_LONG_OPTIONS.contains(&option) && !glued && !negated {
                            index += 1;
                        }
                    }
                    Long::Ambiguous(candidates) => {
                        if !negated
                            && candidates
                                .iter()
                                .any(|option| matches!(*option, "force" | "mirror"))
                        {
                            out.ambiguous = Some(text.to_string());
                        }
                    }
                    Long::Unknown => {}
                }
                continue;
            }
            if text.starts_with('-') && text.len() > 1 {
                let letters: Vec<char> = text[1..].chars().collect();
                for (at, letter) in letters.iter().enumerate() {
                    match letter {
                        'f' => out.force = true,
                        'n' => out.dry_run = true,
                        'o' => {
                            if at + 1 == letters.len() {
                                index += 1;
                            }
                            break;
                        }
                        _ => {}
                    }
                }
                continue;
            }
            positionals.push(arg.clone());
        }
        // The first positional is the repository.
        if !positionals.is_empty() {
            positionals.remove(0);
        }
        out.refspecs = positionals;
        out
    }
}

enum Long {
    Exact(&'static str),
    Unique(&'static str),
    Ambiguous(Vec<&'static str>),
    Unknown,
}

fn resolve_long(name: &str) -> Long {
    if let Some(exact) = LONG_OPTIONS.iter().find(|option| **option == name) {
        return Long::Exact(exact);
    }
    let candidates: Vec<&'static str> = LONG_OPTIONS
        .iter()
        .copied()
        .filter(|option| !name.is_empty() && option.starts_with(name))
        .collect();
    match candidates.as_slice() {
        [] => Long::Unknown,
        [only] => Long::Unique(only),
        _ => Long::Ambiguous(candidates),
    }
}

/// The evidence an opaque node needs: a `push` and a force token
/// (`-f`, `--force`, `--mirror`, `+refspec`).
fn force_push_evidence(text: &str) -> Option<String> {
    crate::model::evidence::find_word(text, "push")?;
    let force = text
        .split(|ch: char| ch.is_whitespace() || "'\"`;|&()".contains(ch))
        .find(|token| {
            matches!(*token, "--force" | "--mirror")
                || (token.starts_with('-')
                    && !token.starts_with("--")
                    && token.len() > 1
                    && token[1..].chars().all(|ch| ch.is_ascii_alphabetic())
                    && token.contains('f'))
                || (token.len() > 1
                    && token.starts_with('+')
                    && token[1..].starts_with(|ch: char| ch.is_ascii_alphanumeric()))
        })?;
    Some(format!("`push` and `{force}`"))
}
