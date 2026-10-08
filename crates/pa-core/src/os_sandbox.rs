//! The session's OS sandbox (the `sandbox` setting, `--sandbox <mode>`): which policy applies,
//! what the model and the user are told about it, and the confined spawns (the Python kernel and
//! the `!` user-bash lane). Plan mode tightens it to `read-only` while it is on
//! ([`SessionSandbox::for_plan_mode`]). The enforcement itself is `pa-os-sandbox`'s.
//!
//! Resolution: the global `sandbox` block is the user's choice; a project file may only tighten
//! it (a stricter `mode`, `network: false`) and never adds writable roots; the CLI flag replaces
//! the mode for one run (`--sandbox off` included). An enabled sandbox this machine cannot
//! enforce is reported as unavailable and refuses to spawn: nothing runs unconfined while the
//! user asked for confinement.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub use pa_os_sandbox::SandboxMode;
use pa_os_sandbox::{
    Assessment, Confinement, NetworkAccess, SandboxError, SandboxPaths, SandboxPolicy,
};

use crate::settings::{SandboxSettings, SettingsManager};

/// A session's enabled sandbox: the policy and what this machine can enforce of it.
#[derive(Debug, Clone)]
pub struct SessionSandbox {
    policy: SandboxPolicy,
    support: Result<Assessment, SandboxError>,
}

/// The configured mode of one scope: unset is `off`, an unrecognized value fails closed.
fn scope_mode(scope: Option<&SandboxSettings>) -> SandboxMode {
    scope
        .and_then(|sandbox| sandbox.mode.as_deref())
        .map_or(SandboxMode::Off, |mode| {
            SandboxMode::from_wire(mode).unwrap_or(SandboxMode::ReadOnly)
        })
}

/// A configured writable root: `~/` expands to the home directory, a relative path resolves
/// against `cwd`.
fn expand_root(root: &str, cwd: &Path) -> Option<PathBuf> {
    let root = root.trim();
    if root.is_empty() {
        return None;
    }
    if root == "~" {
        return pa_types::platform::home_dir();
    }
    if let Some(rest) = root.strip_prefix("~/") {
        return pa_types::platform::home_dir().map(|home| home.join(rest));
    }
    Some(cwd.join(root))
}

impl SessionSandbox {
    /// The session's sandbox, or `None` when the effective mode is `off`.
    #[must_use]
    pub fn resolve(
        settings: &SettingsManager,
        cli_mode: Option<SandboxMode>,
        cwd: &Path,
    ) -> Option<Self> {
        let global = settings.global_settings().sandbox.as_ref();
        let project = settings.project_settings().sandbox.as_ref();
        let mode = cli_mode.unwrap_or_else(|| scope_mode(global).stricter(scope_mode(project)));
        let confinement = mode.confinement()?;
        let network_allowed = global.and_then(|sandbox| sandbox.network) == Some(true)
            && project.and_then(|sandbox| sandbox.network) != Some(false);
        let mut writable_roots: Vec<PathBuf> = global
            .and_then(|sandbox| sandbox.writable_roots.as_deref())
            .unwrap_or_default()
            .iter()
            .filter_map(|root| expand_root(root, cwd))
            .collect();
        // `/tmp` is writable under `workspace-write` like the Codex CLI's default (`$TMPDIR`
        // is part of every mode's scratch).
        writable_roots.push(PathBuf::from("/tmp"));
        let policy = SandboxPolicy {
            confinement,
            network: if network_allowed {
                NetworkAccess::Allowed
            } else {
                NetworkAccess::Denied
            },
            writable_roots,
        };
        let support = pa_os_sandbox::assess(&policy);
        Some(Self { policy, support })
    }

    /// The sandbox plan mode runs under: `configured` tightened to `read-only` (an already
    /// `read-only` one unchanged), keeping its network rule; with no configured sandbox,
    /// `read-only` with network allowed, since plan mode never blocked the network.
    #[must_use]
    pub fn for_plan_mode(configured: Option<&SessionSandbox>) -> SessionSandbox {
        Self::for_plan_mode_with(configured, pa_os_sandbox::assess)
    }

    /// [`Self::for_plan_mode`] with `assess` standing in for this machine's support (tests
    /// inject an unsupported one).
    pub(crate) fn for_plan_mode_with(
        configured: Option<&SessionSandbox>,
        assess: impl Fn(&SandboxPolicy) -> Result<Assessment, SandboxError>,
    ) -> SessionSandbox {
        let policy = match configured {
            Some(configured) => SandboxPolicy {
                confinement: Confinement::ReadOnly,
                ..configured.policy.clone()
            },
            None => SandboxPolicy {
                confinement: Confinement::ReadOnly,
                network: NetworkAccess::Allowed,
                writable_roots: Vec::new(),
            },
        };
        let support = assess(&policy);
        SessionSandbox { policy, support }
    }

    /// Whether `other` enforces the same policy (a spawn under either is confined alike).
    #[must_use]
    pub fn same_policy(&self, other: &SessionSandbox) -> bool {
        self.policy == other.policy
    }

    /// Why this machine cannot enforce the sandbox, or `None` when it can (perhaps degraded).
    #[must_use]
    pub fn unavailable_reason(&self) -> Option<String> {
        self.support.as_ref().err().map(ToString::to_string)
    }

    /// The enforced mode (never [`SandboxMode::Off`]).
    #[must_use]
    pub fn mode(&self) -> SandboxMode {
        self.policy.confinement.mode()
    }

    /// The adoption-telemetry category of an optional sandbox (`off` when none).
    #[must_use]
    pub fn telemetry_mode(sandbox: Option<&SessionSandbox>) -> &'static str {
        sandbox
            .map_or(SandboxMode::Off, SessionSandbox::mode)
            .telemetry_name()
    }

    /// The short status the TUI tray shows after `sandbox `: the mode, `+net` when network is
    /// allowed, and `(degraded)` / `(unavailable)` when this machine cannot enforce all of it.
    #[must_use]
    pub fn status_label(&self) -> String {
        let mut label = self.mode().wire_name().to_string();
        if self.policy.network == NetworkAccess::Allowed {
            label.push_str("+net");
        }
        match &self.support {
            Ok(assessment) if assessment.is_degraded() => label.push_str(" (degraded)"),
            Ok(_) => {}
            Err(_) => label.push_str(" (unavailable)"),
        }
        label
    }

    /// The one system-prompt line telling the model what the sandbox refuses.
    #[must_use]
    pub fn prompt_line(&self) -> String {
        let mode = self.mode().wire_name();
        let assessment = match &self.support {
            Ok(assessment) => assessment,
            Err(error) => {
                return format!(
                    "OS sandbox: `{mode}` was requested but cannot be enforced here ({error}), so \
                     the Python kernel will not start until the user turns it off (`--sandbox off` \
                     or the `sandbox.mode` setting)."
                );
            }
        };
        let writes = match self.policy.confinement {
            Confinement::WorkspaceWrite => {
                "the kernel and every command it runs can write only inside the working \
                 directory, the temp directories and the configured writable roots"
            }
            Confinement::ReadOnly => {
                "the kernel and every command it runs cannot write files outside the temp \
                 directory and the session's own state"
            }
        };
        let network = match self.policy.network {
            NetworkAccess::Allowed => "Network access is allowed.",
            NetworkAccess::Denied => {
                "Network access is blocked (only pipes and unix sockets work)."
            }
        };
        let mut line = format!(
            "OS sandbox: `{mode}` ({}): {writes}; other writes fail with a permission error \
             (EACCES). {network}",
            assessment.mechanism
        );
        if assessment.is_degraded() {
            line.push_str(" Not enforced on this machine: ");
            line.push_str(&assessment.gaps.join("; "));
            line.push('.');
        }
        line
    }

    /// A `Command` running `program` confined to this sandbox, with `workspace` writable under
    /// `workspace-write` and `scratch` (the process's own temp and state directories) writable in
    /// every mode.
    ///
    /// # Errors
    ///
    /// The sandbox is unavailable on this machine, or its rules cannot be built.
    pub fn command(
        &self,
        program: impl AsRef<OsStr>,
        workspace: &Path,
        scratch: Vec<PathBuf>,
    ) -> Result<std::process::Command, SandboxError> {
        Ok(self.prepare(workspace, scratch)?.command(program))
    }

    /// This sandbox prepared for `workspace` and `scratch`, for any number of
    /// spawns: the kernel and the `bash()` commands the host runs for it share
    /// one restriction.
    ///
    /// # Errors
    ///
    /// The sandbox is unavailable on this machine, or its rules cannot be built.
    pub fn prepare(
        &self,
        workspace: &Path,
        scratch: Vec<PathBuf>,
    ) -> Result<pa_os_sandbox::PreparedSandbox, SandboxError> {
        if let Err(error) = &self.support {
            return Err(error.clone());
        }
        let paths = SandboxPaths {
            workspace: workspace.to_path_buf(),
            scratch,
        };
        pa_os_sandbox::prepare(&self.policy, &paths)
    }
}

/// The temp directory a child sees: its `TMPDIR` when set, else the host's.
#[must_use]
pub fn temp_dir_for(env_tmpdir: Option<&str>) -> PathBuf {
    env_tmpdir
        .filter(|dir| !dir.is_empty())
        .map_or_else(std::env::temp_dir, PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(
        global: &str,
        project: &str,
        cli: Option<SandboxMode>,
    ) -> Option<(SandboxMode, NetworkAccess, Vec<PathBuf>)> {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("cwd");
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(cwd.join(".prime/agent")).unwrap();
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("settings.json"), global).unwrap();
        std::fs::write(cwd.join(".prime/agent/settings.json"), project).unwrap();
        let settings = SettingsManager::create(&cwd, &agent_dir);
        SessionSandbox::resolve(&settings, cli, &cwd).map(|sandbox| {
            (
                sandbox.mode(),
                sandbox.policy.network,
                sandbox
                    .policy
                    .writable_roots
                    .iter()
                    .map(|root| {
                        root.strip_prefix(&cwd)
                            .map_or_else(|_| root.clone(), Path::to_path_buf)
                    })
                    .collect(),
            )
        })
    }

    #[test]
    fn off_by_default_and_the_global_block_selects_the_policy() {
        let tmp = PathBuf::from("/tmp");
        assert_eq!(
            [
                resolve("{}", "{}", None),
                resolve(r#"{ "sandbox": { "mode": "off" } }"#, "{}", None),
                resolve(
                    r#"{ "sandbox": { "mode": "workspace-write", "network": true, "writableRoots": ["build"] } }"#,
                    "{}",
                    None
                ),
                resolve(r#"{ "sandbox": { "mode": "read-only" } }"#, "{}", None),
            ],
            [
                None,
                None,
                Some((
                    SandboxMode::WorkspaceWrite,
                    NetworkAccess::Allowed,
                    vec![PathBuf::from("build"), tmp.clone()]
                )),
                Some((SandboxMode::ReadOnly, NetworkAccess::Denied, vec![tmp])),
            ]
        );
    }

    /// A project file tightens (enables, a stricter mode, network off) but never loosens: it
    /// cannot turn the sandbox off, re-enable network, or add writable roots.
    #[test]
    fn a_project_scope_only_tightens() {
        let tmp = PathBuf::from("/tmp");
        let user = r#"{ "sandbox": { "mode": "workspace-write", "network": true } }"#;
        assert_eq!(
            [
                resolve(
                    "{}",
                    r#"{ "sandbox": { "mode": "workspace-write", "network": true } }"#,
                    None
                ),
                resolve(user, r#"{ "sandbox": { "mode": "off" } }"#, None),
                resolve(
                    user,
                    r#"{ "sandbox": { "mode": "read-only", "network": false, "writableRoots": ["/"] } }"#,
                    None
                ),
            ],
            [
                Some((
                    SandboxMode::WorkspaceWrite,
                    NetworkAccess::Denied,
                    vec![tmp.clone()]
                )),
                Some((
                    SandboxMode::WorkspaceWrite,
                    NetworkAccess::Allowed,
                    vec![tmp.clone()]
                )),
                Some((SandboxMode::ReadOnly, NetworkAccess::Denied, vec![tmp])),
            ]
        );
    }

    /// `--sandbox` replaces the configured mode for the run, `off` included; an unrecognized
    /// configured mode fails closed to `read-only`.
    #[test]
    fn the_cli_mode_overrides_and_an_unknown_mode_fails_closed() {
        let user = r#"{ "sandbox": { "mode": "workspace-write" } }"#;
        let mode = |global: &str, cli| resolve(global, "{}", cli).map(|resolved| resolved.0);
        assert_eq!(
            [
                mode(user, Some(SandboxMode::Off)),
                mode("{}", Some(SandboxMode::ReadOnly)),
                mode(r#"{ "sandbox": { "mode": "workspace_write" } }"#, None),
            ],
            [
                None,
                Some(SandboxMode::ReadOnly),
                Some(SandboxMode::ReadOnly)
            ]
        );
    }

    /// Plan mode tightens any configured mode to `read-only` and keeps its network rule; with
    /// the sandbox off it is `read-only` with network allowed.
    #[test]
    fn plan_mode_tightens_to_read_only_and_keeps_the_network_rule() {
        let full = |_: &SandboxPolicy| {
            Ok(Assessment {
                mechanism: "Landlock ABI 6".to_string(),
                gaps: Vec::new(),
            })
        };
        let configured = |confinement, network| SessionSandbox {
            policy: SandboxPolicy {
                confinement,
                network,
                writable_roots: vec![PathBuf::from("/tmp")],
            },
            support: full(&SandboxPolicy {
                confinement,
                network,
                writable_roots: Vec::new(),
            }),
        };
        let workspace_write = configured(Confinement::WorkspaceWrite, NetworkAccess::Denied);
        let read_only = configured(Confinement::ReadOnly, NetworkAccess::Allowed);
        let plans = [
            SessionSandbox::for_plan_mode_with(None, full),
            SessionSandbox::for_plan_mode_with(Some(&workspace_write), full),
            SessionSandbox::for_plan_mode_with(Some(&read_only), full),
        ];
        let described: Vec<(SandboxMode, NetworkAccess)> = plans
            .iter()
            .map(|plan| (plan.mode(), plan.policy.network))
            .collect();
        assert_eq!(
            described,
            vec![
                (SandboxMode::ReadOnly, NetworkAccess::Allowed),
                (SandboxMode::ReadOnly, NetworkAccess::Denied),
                (SandboxMode::ReadOnly, NetworkAccess::Allowed),
            ]
        );
        // An already read-only sandbox is the plan policy itself: nothing to restart into.
        assert_eq!(
            (
                plans[1].same_policy(&workspace_write),
                plans[2].same_policy(&read_only)
            ),
            (false, true)
        );
        let unsupported = SessionSandbox::for_plan_mode_with(None, |_| {
            Err(SandboxError::Unsupported {
                reason: "no Landlock".to_string(),
            })
        });
        assert_eq!(
            (
                plans[0].unavailable_reason(),
                unsupported.unavailable_reason()
            ),
            (
                None,
                Some("OS sandbox unavailable: no Landlock".to_string())
            )
        );
    }

    #[test]
    fn labels_and_prompt_lines_name_the_mode_and_the_machine_support() {
        let sandbox = |confinement, network, support| SessionSandbox {
            policy: SandboxPolicy {
                confinement,
                network,
                writable_roots: Vec::new(),
            },
            support,
        };
        let full = Ok(Assessment {
            mechanism: "Landlock ABI 6 + seccomp".to_string(),
            gaps: Vec::new(),
        });
        let degraded = Ok(Assessment {
            mechanism: "Landlock ABI 2".to_string(),
            gaps: vec!["truncating existing files is not confined".to_string()],
        });
        let unavailable = Err(SandboxError::Unsupported {
            reason: "not on this platform".to_string(),
        });
        let cases = [
            sandbox(Confinement::WorkspaceWrite, NetworkAccess::Denied, full),
            sandbox(Confinement::ReadOnly, NetworkAccess::Allowed, degraded),
            sandbox(
                Confinement::WorkspaceWrite,
                NetworkAccess::Denied,
                unavailable,
            ),
        ];
        let rendered: Vec<(String, String)> = cases
            .iter()
            .map(|case| (case.status_label(), case.prompt_line()))
            .collect();
        assert_eq!(
            rendered,
            vec![
                (
                    "workspace-write".to_string(),
                    "OS sandbox: `workspace-write` (Landlock ABI 6 + seccomp): the kernel and every \
                     command it runs can write only inside the working directory, the temp \
                     directories and the configured writable roots; other writes fail with a \
                     permission error (EACCES). Network access is blocked (only pipes and unix \
                     sockets work)."
                        .to_string()
                ),
                (
                    "read-only+net (degraded)".to_string(),
                    "OS sandbox: `read-only` (Landlock ABI 2): the kernel and every command it runs \
                     cannot write files outside the temp directory and the session's own state; \
                     other writes fail with a permission error (EACCES). Network access is \
                     allowed. Not enforced on this machine: truncating existing files is not \
                     confined."
                        .to_string()
                ),
                (
                    "workspace-write (unavailable)".to_string(),
                    "OS sandbox: `workspace-write` was requested but cannot be enforced here (OS \
                     sandbox unavailable: not on this platform), so the Python kernel will not \
                     start until the user turns it off (`--sandbox off` or the `sandbox.mode` \
                     setting)."
                        .to_string()
                ),
            ]
        );
    }
}
