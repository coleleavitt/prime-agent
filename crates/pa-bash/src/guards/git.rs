//! What the git rules share: reading a `git` invocation's global options and
//! subcommand, and replaying them in a read-only probe.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::context::GuardContext;
use crate::model::{Arg, Invocation};
use crate::probe::{ProbeLimits, ProbeOutcome, run_probe};
use crate::script::Script;

/// A `git` invocation, split at its subcommand.
#[derive(Debug, Clone)]
pub(super) struct GitCall<'a> {
    pub invocation: &'a Invocation,
    /// The global options before the subcommand, as written.
    pub global: &'a [Arg],
    /// `-c key=value` operands.
    pub configs: Vec<&'a Arg>,
    /// The subcommand (`None` for `git --version` and the like).
    pub subcommand: Option<&'a Arg>,
    /// Everything after the subcommand.
    pub args: &'a [Arg],
}

const VALUE_OPTIONS: [&str; 8] = [
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
    "--config-env",
    "--list-cmds",
];

/// Whether `invocation` runs git (`git`, `/usr/bin/git`, `GIT`, `git.exe`).
pub(super) fn is_git(invocation: &Invocation) -> bool {
    invocation
        .program()
        .is_some_and(|name| matches!(name.to_ascii_lowercase().as_str(), "git" | "git.exe"))
}

pub(super) fn git_call(invocation: &Invocation) -> Option<GitCall<'_>> {
    if !is_git(invocation) {
        return None;
    }
    let argv = &invocation.argv;
    let mut configs = Vec::new();
    let mut index = 1;
    while index < argv.len() {
        let Some(text) = argv[index].known() else {
            break;
        };
        if !text.starts_with('-') {
            break;
        }
        if text == "--" {
            return Some(GitCall {
                invocation,
                global: &argv[1..index],
                configs,
                subcommand: None,
                args: &[],
            });
        }
        if matches!(
            text,
            "--exec-path"
                | "--html-path"
                | "--man-path"
                | "--info-path"
                | "--version"
                | "-v"
                | "-h"
                | "--help"
        ) {
            return Some(GitCall {
                invocation,
                global: &argv[1..index],
                configs,
                subcommand: None,
                args: &[],
            });
        }
        if VALUE_OPTIONS.contains(&text) {
            if text == "-c" {
                if let Some(value) = argv.get(index + 1) {
                    configs.push(value);
                }
            }
            index += 2;
            continue;
        }
        if text.len() > 2 && text.starts_with("-c") {
            configs.push(&argv[index]);
        }
        index += 1;
    }
    Some(GitCall {
        invocation,
        global: &argv[1..index.min(argv.len())],
        configs,
        subcommand: argv.get(index),
        args: argv.get(index + 1..).unwrap_or(&[]),
    })
}

impl GitCall<'_> {
    pub(super) fn subcommand_is(&self, name: &str) -> bool {
        self.subcommand.and_then(Arg::known) == Some(name)
    }

    /// The `-c` values as text (`-ckey=value` reads as `key=value`).
    pub(super) fn config_values(&self) -> Vec<Option<String>> {
        self.configs
            .iter()
            .map(|arg| {
                arg.known().map(|text| {
                    text.strip_prefix("-c")
                        .filter(|_| text.starts_with("-c") && text.len() > 2)
                        .unwrap_or(text)
                        .to_string()
                })
            })
            .collect()
    }

    /// Whether every global option and assignment can be replayed in a
    /// probe.
    pub(super) fn replayable(&self) -> bool {
        self.global.iter().all(|arg| arg.known().is_some())
            && self
                .invocation
                .env
                .iter()
                .all(|(_, value)| value.known().is_some())
            && self
                .invocation
                .git_env
                .iter()
                .all(|(_, value)| value.as_ref().is_none_or(|value| value.known().is_some()))
    }

    /// A shell command running `git <global options> <tail>` with the
    /// invocation's repository-selecting assignments, after the trusted
    /// `prefix`, in which git cannot run anything the command or a
    /// configuration names: see [`NEUTRAL_ENV`], [`NEUTRAL_CONFIG`] and
    /// [`executes`].
    pub(super) fn probe_command(&self, tail: &str, prefix: Option<&str>) -> String {
        let mut line = String::from(NEUTRAL_ENV);
        // `GIT_*` variables the script set or unset before the command.
        let mut unset = String::new();
        for (name, value) in &self.invocation.git_env {
            if !REPLAYED_ENV.contains(&name.as_str()) && !name.starts_with("GIT_CONFIG_") {
                continue;
            }
            match value.as_ref().and_then(Arg::known) {
                Some(value) => {
                    let _ = write!(line, "{name}={} ", quote(value));
                }
                None => {
                    let _ = write!(unset, "unset {name}; ");
                }
            }
        }
        for (name, value) in &self.invocation.env {
            if !REPLAYED_ENV.contains(&name.as_str()) && !name.starts_with("GIT_CONFIG_") {
                continue;
            }
            if let Some(value) = value.known() {
                let _ = write!(line, "{name}={} ", quote(value));
            }
        }
        let mut git = String::from("command git");
        // `--config-env=KEY=VAR` reads VAR from the command's environment,
        // which is not replayed: its value moves to a variable of the probe's
        // own (`PA_CONFIG_ENV_<n>`), so no other assignment reaches git.
        let config_env = |key: &str, var: &str, git: &mut String, line: &mut String| {
            if executes(key) {
                return;
            }
            let value = self
                .invocation
                .env
                .iter()
                .rev()
                .find(|(name, _)| name == var)
                .and_then(|(_, value)| value.known().map(str::to_string));
            let renamed = format!("PA_CONFIG_ENV_{}", line.matches("PA_CONFIG_ENV_").count());
            match value {
                Some(value) => {
                    let _ = write!(line, "{renamed}={} ", quote(&value));
                }
                None => {
                    let _ = write!(line, "{renamed}=\"${{{var}}}\" ");
                }
            }
            let _ = write!(git, " --config-env={}", quote(&format!("{key}={renamed}")));
        };
        let mut global = self.global.iter();
        while let Some(arg) = global.next() {
            let Some(text) = arg.known() else {
                continue;
            };
            if text == "-p" || text == "--paginate" || text.starts_with("--exec-path") {
                continue;
            }
            if text == "-c" {
                let value = global.next().and_then(Arg::known).unwrap_or_default();
                if !executes(value.split('=').next().unwrap_or_default()) {
                    let _ = write!(git, " -c {}", quote(value));
                }
                continue;
            }
            let config = if text == "--config-env" {
                global.next().and_then(Arg::known).unwrap_or_default()
            } else if let Some(value) = text.strip_prefix("--config-env=") {
                value
            } else {
                git.push(' ');
                git.push_str(&quote(text));
                continue;
            };
            if let Some((key, var)) = config.split_once('=') {
                if crate::model::wrappers::is_name(var) {
                    config_env(key, var, &mut git, &mut line);
                }
            }
        }
        // Filter and diff drivers have names of their own: blank every one
        // the effective configuration defines, then the fixed keys.
        let function = format!(
            "pa_git() {{\n\
             \tpa_keys=$({line}{git} config --name-only --get-regexp {drivers} 2>/dev/null)\n\
             \tpa_old=$IFS; IFS='\n'\n\
             \tfor pa_key in $pa_keys; do set -- -c \"$pa_key=\" \"$@\"; done\n\
             \tIFS=$pa_old\n\
             \t{line}{git} {NEUTRAL_CONFIG} \"$@\"\n\
             }}\n",
            drivers = quote(DRIVER_KEYS),
        );
        let body = format!("{unset}{function}{}", tail.replace("{git}", "pa_git"));
        Script::compose(&body, prefix.filter(|prefix| !prefix.is_empty()))
    }
}

/// The environment of every git probe: no optional lock (the probe never
/// writes the index, so no `post-index-change` hook), no prompt, and every
/// program git might start through the environment disarmed.
const NEUTRAL_ENV: &str = "GIT_OPTIONAL_LOCKS=0 GIT_TERMINAL_PROMPT=0 GIT_PAGER=cat PAGER=cat GIT_EDITOR=false \
VISUAL=false EDITOR=false GIT_ASKPASS=false SSH_ASKPASS=false GIT_SSH_COMMAND=false GIT_SSH=false \
GIT_EXTERNAL_DIFF= GIT_PROXY_COMMAND= GIT_ALLOW_PROTOCOL=none GIT_PROTOCOL_FROM_USER=0 GIT_ATTR_NOSYSTEM=1 ";

/// `-c` overrides given last, which win over every other scope (repository,
/// global, system, `GIT_CONFIG_*`): each key that names a program git runs.
const NEUTRAL_CONFIG: &str = "-c core.fsmonitor=false -c core.hooksPath=/dev/null -c core.sshCommand=false \
-c core.pager=cat -c core.editor=false -c sequence.editor=false -c core.askPass=false -c credential.helper= \
-c diff.external= -c gpg.program=false -c gpg.ssh.program=false -c gpg.x509.program=false \
-c gpg.ssh.defaultKeyCommand=false -c core.alternateRefsCommand= -c core.gitProxy= \
-c uploadpack.packObjectsHook= -c gc.recentObjectsHook= -c core.attributesFile=/dev/null \
-c protocol.allow=never -c submodule.recurse=false";

/// The configuration keys that name filter or diff driver programs.
const DRIVER_KEYS: &str =
    "^(filter|diff|merge)\\..*\\.(clean|smudge|process|textconv|command|driver)$";

/// The repository-selecting variables a probe replays.
const REPLAYED_ENV: [&str; 9] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// Whether the git configuration key `key` names a program git runs (or a
/// file that can define one): a probe never replays it.
pub(super) fn executes(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let section = key.split('.').next().unwrap_or_default();
    let last = key.rsplit('.').next().unwrap_or_default();
    matches!(
        section,
        "include"
            | "includeif"
            | "pager"
            | "credential"
            | "gpg"
            | "sendemail"
            | "browser"
            | "man"
            | "mergetool"
            | "difftool"
            | "protocol"
            | "url"
            | "trailer"
    ) || matches!(
        last,
        "fsmonitor"
            | "hookspath"
            | "sshcommand"
            | "pager"
            | "editor"
            | "askpass"
            | "external"
            | "program"
            | "command"
            | "cmd"
            | "helper"
            | "textconv"
            | "clean"
            | "smudge"
            | "process"
            | "driver"
            | "alternaterefscommand"
            | "gitproxy"
            | "proxy"
            | "packobjectshook"
            | "recentobjectshook"
            | "uploadpack"
            | "receivepack"
            | "attributesfile"
    )
}

/// `text` single-quoted for sh.
pub(super) fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The directories `invocation` may run git in, or the relocation that hides
/// them.
pub(super) fn directories(invocation: &Invocation) -> Result<&[PathBuf], String> {
    match (&invocation.cwd.unknown, invocation.cwd.dirs.as_slice()) {
        (Some(unknown), _) => Err(unknown.clone()),
        (None, []) => Err("an unknown directory".to_string()),
        (None, dirs) => Ok(dirs),
    }
}

/// Run a probe in `dir`: `Some(stdout)` when it exited 0 (or its output was
/// cut at the cap), `None` when it failed, `Err` when it timed out.
pub(super) fn probe(
    context: &GuardContext,
    command: &str,
    dir: &Path,
    limits: ProbeLimits,
) -> Result<Option<(String, bool)>, ()> {
    if !dir.is_dir() {
        return Ok(None);
    }
    match run_probe(context, command, dir, limits) {
        ProbeOutcome::Finished {
            status,
            stdout,
            truncated,
        } => {
            if status != Some(0) && !truncated {
                return Ok(None);
            }
            Ok(Some((
                String::from_utf8_lossy(&stdout).into_owned(),
                truncated,
            )))
        }
        ProbeOutcome::TimedOut => Err(()),
        ProbeOutcome::Unavailable => Ok(None),
    }
}

pub(super) const STATUS_LIMITS: ProbeLimits = ProbeLimits {
    timeout: Duration::from_secs(10),
    kill_grace: Duration::from_secs(1),
    output_cap: Some(64 * 1024),
};

pub(super) const UPSTREAM_LIMITS: ProbeLimits = ProbeLimits {
    timeout: Duration::from_secs(2),
    kill_grace: Duration::from_secs(1),
    output_cap: None,
};
