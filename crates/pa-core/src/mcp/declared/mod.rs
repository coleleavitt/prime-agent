//! Where MCP servers are declared, and which of them run.
//!
//! Besides the Prime Agent settings' `mcpServers`, servers come the way
//! Claude Code declares them:
//!
//! | scope | file | approval |
//! |---|---|---|
//! | `settings` | `<agentDir>/settings.json`, the project's `.prime/agent/settings.json` (trusted) | none |
//! | `local` | `~/.claude.json`'s `projects[<project>].mcpServers` (opt-in) | none |
//! | `project` | every `.mcp.json` from the filesystem root down to the working directory | required |
//! | `user` | `~/.claude.json`'s `mcpServers` (opt-in) | none |
//!
//! A name declared in several scopes takes the first one in that order
//! (Claude Code's local > project > user, under this product's own
//! settings); a nearer `.mcp.json` overrides a farther one. A project
//! server runs only once approved ([`approval`]); until then it is listed
//! (pending or rejected) but never started, and a pending one is hidden
//! when another scope declares its name. The `~/.claude.json` scopes load
//! only with the user settings' `"permissions": {"claudeCodeSettings":
//! true}` opt-in.
//!
//! Claude Code entries expand `${VAR}` and `${VAR:-default}` as they load
//! ([`expand`]), reading the host environment under the kernel's
//! `kernel.environment` policy, and report what they could not load as
//! [`McpConfigIssue`]s with Claude Code's wording.

mod approval;
mod ask;
mod expand;
mod mcp_json;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use approval::{
    PROJECT_CHOICES_FILE,
    ProjectChoice,
    ProjectChoices,
    read_project_choices,
    record_project_choice,
    reset_project_choices,
    same_server_name,
};
pub use ask::ask_pending_project_servers;
pub use expand::{Expansion, expand_env_vars};
pub use mcp_json::MCP_CONFIG_MAX_BYTES;
use serde::Serialize;
use serde_json::Value;

use super::McpServerConfig;
use crate::settings::SettingsManager;

/// The project MCP config file name.
pub const MCP_JSON_FILE: &str = ".mcp.json";

/// Where a server is declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerScope {
    /// The Prime Agent settings' `mcpServers`.
    Settings,
    /// `~/.claude.json`, this project's entry.
    Local,
    /// A `.mcp.json` file.
    Project,
    /// `~/.claude.json`, every project.
    User,
}

impl McpServerScope {
    /// The precedence order, first wins.
    const ORDER: [McpServerScope; 4] = [
        McpServerScope::Settings,
        McpServerScope::Local,
        McpServerScope::Project,
        McpServerScope::User,
    ];

    /// The wire name.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            McpServerScope::Settings => "settings",
            McpServerScope::Local => "local",
            McpServerScope::Project => "project",
            McpServerScope::User => "user",
        }
    }

    /// How the scope is described to the user (Claude Code's wording for
    /// its own scopes).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            McpServerScope::Settings => "Prime Agent settings",
            McpServerScope::Local => "Local config (private to you in this project)",
            McpServerScope::Project => "Project config (shared via .mcp.json)",
            McpServerScope::User => "User config (available in all your projects)",
        }
    }

    fn rank(self) -> usize {
        Self::ORDER
            .iter()
            .position(|scope| *scope == self)
            .unwrap_or(Self::ORDER.len())
    }
}

/// A project server's approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectApproval {
    Approved,
    /// Not decided: listed, never started.
    Pending,
    /// Declined (`disabledMcpjsonServers`): listed, never started.
    Rejected,
}

/// How bad a config problem is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    /// The whole file was not loaded.
    Fatal,
    /// One server was skipped, or loaded with a caveat.
    Warning,
}

/// One problem found while loading MCP server declarations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConfigIssue {
    /// The file it is in; `None` for the settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The JSON path (`mcpServers.<name>`), empty for the whole file.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    pub severity: IssueSeverity,
}

/// One declaration of a server.
#[derive(Debug, Clone, PartialEq)]
pub struct DeclaredServer {
    pub name: String,
    pub config: McpServerConfig,
    pub scope: McpServerScope,
    /// The file that declares it.
    pub source: PathBuf,
    /// The approval of a project server; `None` for the other scopes.
    pub approval: Option<ProjectApproval>,
}

impl DeclaredServer {
    /// Whether it may run.
    #[must_use]
    pub fn runnable(&self) -> bool {
        match self.approval {
            None | Some(ProjectApproval::Approved) => true,
            Some(ProjectApproval::Pending | ProjectApproval::Rejected) => false,
        }
    }
}

/// One `/mcp` row's declaration facts (the daemon's `get_mcp_connections`
/// response carries them as `declaredServers`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredServerView {
    pub name: String,
    pub scope: McpServerScope,
    pub scope_label: &'static str,
    /// The declaring file.
    pub source: String,
    /// `http` or `stdio`.
    pub transport: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<ProjectApproval>,
    /// The same name's lower-precedence declarations this one overrides.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<McpServerScope>,
}

/// Claude Code's user files the opt-in reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeFiles {
    /// `~/.claude.json` (`$CLAUDE_CONFIG_DIR/.claude.json`; a legacy
    /// `~/.claude/.config.json` wins when present).
    pub global_config: PathBuf,
    /// `~/.claude/settings.json` (`$CLAUDE_CONFIG_DIR/settings.json`).
    pub user_settings: PathBuf,
}

impl ClaudeCodeFiles {
    /// Locate the files the way Claude Code does; `None` without a home.
    #[must_use]
    pub fn locate() -> Option<Self> {
        let user_dir = crate::skills::claude_commands::user_claude_dir()?;
        let legacy = user_dir.join(".config.json");
        let global_config = if legacy.is_file() {
            legacy
        } else {
            match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty()) {
                Some(dir) => PathBuf::from(dir).join(".claude.json"),
                None => pa_types::platform::home_dir()?.join(".claude.json"),
            }
        };
        Some(ClaudeCodeFiles {
            global_config,
            user_settings: user_dir.join("settings.json"),
        })
    }
}

/// Whether the user settings opt into Claude Code's files
/// (`"permissions": {"claudeCodeSettings": true}`, the opt-in the
/// permission rules read).
#[must_use]
pub fn claude_code_opt_in(settings: &SettingsManager) -> bool {
    settings
        .global_settings()
        .extra
        .get("permissions")
        .and_then(|permissions| permissions.get("claudeCodeSettings"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// What [`DeclaredServers::load_with`] reads.
pub struct LoadInputs<'a> {
    pub cwd: &'a Path,
    pub agent_dir: &'a Path,
    pub settings: &'a SettingsManager,
    /// Claude Code's user files; `None` skips them (no opt-in).
    pub claude_code: Option<ClaudeCodeFiles>,
    /// A host variable's value for `${VAR}` expansion.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// The farthest directory whose `.mcp.json` is read; `None` walks up
    /// to (not including) the filesystem root, Claude Code's walk.
    pub walk_stop: Option<&'a Path>,
}

/// Every declared server of a workspace, with its approval, and the
/// problems found loading them.
#[derive(Debug, Clone, Default)]
pub struct DeclaredServers {
    /// The canonical workspace (the approval store's key).
    pub workspace: PathBuf,
    /// The agent dir (the approval store's home).
    pub agent_dir: PathBuf,
    servers: Vec<DeclaredServer>,
    issues: Vec<McpConfigIssue>,
}

/// Claude Code's `~/.claude.json` project key: the canonical git root
/// (a linked worktree's main repository), else the canonical directory.
fn claude_project_key(cwd: &Path) -> String {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let root = git_root(&canonical).unwrap_or(canonical);
    let key = root.display().to_string();
    if cfg!(windows) {
        key.replace('\\', "/")
    } else {
        key
    }
}

fn git_root(dir: &Path) -> Option<PathBuf> {
    for ancestor in dir.ancestors() {
        let dot_git = ancestor.join(".git");
        if dot_git.is_dir() {
            return Some(ancestor.to_path_buf());
        }
        if dot_git.is_file() {
            // A linked worktree: `gitdir: <main>/.git/worktrees/<name>`.
            let main = std::fs::read_to_string(&dot_git)
                .ok()
                .and_then(|text| {
                    text.lines()
                        .find_map(|line| line.strip_prefix("gitdir:"))
                        .map(|gitdir| ancestor.join(gitdir.trim()))
                })
                .and_then(|gitdir| {
                    let worktrees = gitdir.parent()?;
                    (worktrees.file_name()? == "worktrees")
                        .then(|| worktrees.parent()?.parent().map(Path::to_path_buf))
                        .flatten()
                })
                .and_then(|main| std::fs::canonicalize(main).ok());
            return Some(main.unwrap_or_else(|| ancestor.to_path_buf()));
        }
    }
    None
}

impl DeclaredServers {
    /// The settings' servers alone (an embedding without a workspace).
    #[must_use]
    pub fn from_settings_servers(servers: HashMap<String, McpServerConfig>) -> Self {
        let mut servers: Vec<DeclaredServer> = servers
            .into_iter()
            .map(|(name, config)| DeclaredServer {
                name,
                config,
                scope: McpServerScope::Settings,
                source: PathBuf::new(),
                approval: None,
            })
            .collect();
        servers.sort_by(|left, right| left.name.cmp(&right.name));
        DeclaredServers {
            servers,
            ..DeclaredServers::default()
        }
    }

    /// Load the servers of the workspace at `cwd` under `settings` (the
    /// workspace's trust-gated settings): the Claude Code files when the
    /// user opted in, `${VAR}` read from the host environment under the
    /// `kernel.environment` policy.
    #[must_use]
    pub fn load(cwd: &Path, agent_dir: &Path, settings: &SettingsManager) -> Self {
        let policy = settings.get_kernel_environment();
        let env = move |name: &str| -> Option<String> {
            if policy.passes_to_kernel(name) {
                std::env::var(name).ok()
            } else {
                None
            }
        };
        Self::load_with(&LoadInputs {
            cwd,
            agent_dir,
            settings,
            claude_code: claude_code_opt_in(settings)
                .then(ClaudeCodeFiles::locate)
                .flatten(),
            env: &env,
            walk_stop: None,
        })
    }

    /// [`Self::load`] over explicit inputs.
    #[must_use]
    pub fn load_with(inputs: &LoadInputs<'_>) -> Self {
        let workspace = crate::workspace_trust::workspace_key(inputs.cwd);
        let credential_vars = pa_ai::env_api_keys::provider_api_key_env_vars();
        let credential = |name: &str| credential_vars.contains(&name);
        let env = mcp_json::EnvLookup {
            value: inputs.env,
            credential: &credential,
        };
        let mut declared = DeclaredServers {
            workspace: workspace.clone(),
            agent_dir: inputs.agent_dir.to_path_buf(),
            servers: Vec::new(),
            issues: Vec::new(),
        };
        declared.load_settings_servers(inputs);
        declared.load_project_files(inputs.cwd, inputs.walk_stop, &env);
        if let Some(files) = &inputs.claude_code {
            declared.load_claude_config(&files.global_config, inputs.cwd, &env);
        }
        let approvals = approval::Approvals::collect(
            inputs.settings,
            inputs.agent_dir,
            inputs.cwd,
            &workspace,
            inputs.claude_code.as_ref(),
        );
        for server in &mut declared.servers {
            if server.scope == McpServerScope::Project {
                server.approval = Some(approvals.decide(&server.name));
            }
        }
        declared
    }

    fn load_settings_servers(&mut self, inputs: &LoadInputs<'_>) {
        let settings = inputs.settings;
        let project_servers = settings.project_settings().mcp_servers.clone();
        let global_file = inputs.agent_dir.join("settings.json");
        let project_file = inputs
            .cwd
            .join(crate::settings::CONFIG_DIR_NAME)
            .join("settings.json");
        let mut servers: Vec<(String, Value)> = settings
            .settings()
            .mcp_servers
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        servers.sort_by(|left, right| left.0.cmp(&right.0));
        for (name, value) in servers {
            let source = if project_servers
                .as_ref()
                .is_some_and(|project| project.contains_key(&name))
            {
                project_file.clone()
            } else {
                global_file.clone()
            };
            match serde_json::from_value::<McpServerConfig>(value) {
                Ok(config) => self.servers.push(DeclaredServer {
                    name,
                    config,
                    scope: McpServerScope::Settings,
                    source,
                    approval: None,
                }),
                Err(error) => self.issues.push(McpConfigIssue {
                    file: Some(source.display().to_string()),
                    path: format!("mcpServers.{name}"),
                    server: Some(name.clone()),
                    message: format!(
                        "Skipped \u{2014} invalid MCP server config for \"{name}\": {error}"
                    ),
                    suggestion: None,
                    severity: IssueSeverity::Warning,
                }),
            }
        }
    }

    /// Every `.mcp.json` from the filesystem root down to `cwd` (Claude
    /// Code's walk: the root directory itself is not read); a nearer file's
    /// server overrides a farther one's.
    fn load_project_files(
        &mut self,
        cwd: &Path,
        stop: Option<&Path>,
        env: &mcp_json::EnvLookup<'_>,
    ) {
        let mut directories: Vec<&Path> = Vec::new();
        for directory in cwd.ancestors() {
            if stop.is_none() && directory.parent().is_none() {
                break;
            }
            directories.push(directory);
            if stop == Some(directory) {
                break;
            }
        }
        directories.reverse();
        let mut project: Vec<DeclaredServer> = Vec::new();
        for directory in directories {
            let file = directory.join(MCP_JSON_FILE);
            let document = match mcp_json::read_config_file(&file) {
                mcp_json::FileRead::Absent => continue,
                mcp_json::FileRead::Failed(issue) => {
                    self.issues.push(issue);
                    continue;
                }
                mcp_json::FileRead::Read(document) => document,
            };
            let parsed = mcp_json::parse_mcp_json(&document, &file, env);
            self.issues.extend(parsed.issues);
            for (name, config) in parsed.servers {
                project.retain(|server| server.name != name);
                project.push(DeclaredServer {
                    name,
                    config,
                    scope: McpServerScope::Project,
                    source: file.clone(),
                    approval: None,
                });
            }
        }
        project.sort_by(|left, right| left.name.cmp(&right.name));
        self.servers.extend(project);
    }

    /// `~/.claude.json`: this project's servers (`local`) and everyone's
    /// (`user`).
    fn load_claude_config(&mut self, file: &Path, cwd: &Path, env: &mcp_json::EnvLookup<'_>) {
        let document = match mcp_json::read_config_file(file) {
            mcp_json::FileRead::Absent => return,
            mcp_json::FileRead::Failed(issue) => {
                self.issues.push(issue);
                return;
            }
            mcp_json::FileRead::Read(document) => document,
        };
        let key = claude_project_key(cwd);
        let local = document
            .get("projects")
            .and_then(|projects| projects.get(&key))
            .and_then(|project| project.get("mcpServers"))
            .and_then(Value::as_object);
        let user = document.get("mcpServers").and_then(Value::as_object);
        for (scope, servers) in [(McpServerScope::Local, local), (McpServerScope::User, user)] {
            let Some(servers) = servers else {
                continue;
            };
            let parsed = mcp_json::parse_servers(servers, Some(file), env);
            self.issues.extend(parsed.issues);
            self.servers.extend(
                parsed
                    .servers
                    .into_iter()
                    .map(|(name, config)| DeclaredServer {
                        name,
                        config,
                        scope,
                        source: file.to_path_buf(),
                        approval: None,
                    }),
            );
        }
    }

    /// Every declaration, in load order.
    #[must_use]
    pub fn all(&self) -> &[DeclaredServer] {
        &self.servers
    }

    /// The problems found loading them.
    #[must_use]
    pub fn issues(&self) -> &[McpConfigIssue] {
        &self.issues
    }

    /// The declarations of each name, highest precedence first, names in
    /// order.
    fn by_name(&self) -> Vec<(&str, Vec<&DeclaredServer>)> {
        let mut names: Vec<&str> = self
            .servers
            .iter()
            .map(|server| server.name.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                let mut declarations: Vec<&DeclaredServer> = self
                    .servers
                    .iter()
                    .filter(|server| server.name == name)
                    .collect();
                declarations.sort_by_key(|server| server.scope.rank());
                (name, declarations)
            })
            .collect()
    }

    /// The declaration each name shows as: the highest-precedence one that
    /// may run, else the unapproved project one (when no other scope
    /// declares the name), else none.
    #[must_use]
    pub fn shown(&self) -> Vec<&DeclaredServer> {
        self.by_name()
            .into_iter()
            .filter_map(|(_, declarations)| {
                declarations
                    .iter()
                    .find(|server| server.runnable())
                    .or_else(|| (declarations.len() == 1).then(|| &declarations[0]))
                    .copied()
            })
            .collect()
    }

    /// The servers that may run, by name.
    #[must_use]
    pub fn dispatchable(&self) -> HashMap<String, McpServerConfig> {
        self.shown()
            .into_iter()
            .filter(|server| server.runnable())
            .map(|server| (server.name.clone(), server.config.clone()))
            .collect()
    }

    /// The project servers waiting for the user's approval.
    #[must_use]
    pub fn pending(&self) -> Vec<&DeclaredServer> {
        self.shown()
            .into_iter()
            .filter(|server| server.approval == Some(ProjectApproval::Pending))
            .collect()
    }

    /// The shown declaration of `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&DeclaredServer> {
        self.shown().into_iter().find(|server| server.name == name)
    }

    /// The `/mcp` rows' declaration facts.
    #[must_use]
    pub fn views(&self) -> Vec<DeclaredServerView> {
        let by_name = self.by_name();
        self.shown()
            .into_iter()
            .map(|server| {
                let overrides = by_name
                    .iter()
                    .find(|(name, _)| *name == server.name)
                    .map(|(_, declarations)| {
                        declarations
                            .iter()
                            .filter(|other| other.scope.rank() > server.scope.rank())
                            .map(|other| other.scope)
                            .collect()
                    })
                    .unwrap_or_default();
                DeclaredServerView {
                    name: server.name.clone(),
                    scope: server.scope,
                    scope_label: server.scope.label(),
                    source: server.source.display().to_string(),
                    transport: server.config.server_type(),
                    approval: server.approval,
                    overrides,
                }
            })
            .collect()
    }

    /// Why `name` cannot be used, when it is a project server waiting for
    /// approval or one the user rejected.
    #[must_use]
    pub fn unavailable_reason(&self, name: &str) -> Option<String> {
        let server = self.get(name)?;
        match server.approval? {
            ProjectApproval::Approved => None,
            ProjectApproval::Pending => Some(format!(
                "\"{name}\" is a project-scope MCP server ({MCP_JSON_FILE}) that is not \
                 approved for this project \u{2014} approve it via /mcp first"
            )),
            ProjectApproval::Rejected => Some(format!(
                "\"{name}\" comes with this repository ({MCP_JSON_FILE}) and you rejected it; \
                 `prime-agent mcp reset-project-choices` asks again"
            )),
        }
    }
}
