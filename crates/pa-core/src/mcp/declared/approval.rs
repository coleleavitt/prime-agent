//! Project server approval, Claude Code's rule: a `.mcp.json` server runs
//! only once approved. A server named in `disabledMcpjsonServers` is
//! rejected; one named in `enabledMcpjsonServers`, or any server when
//! `enableAllProjectMcpServers` is set, is approved; every other one is
//! pending (listed, never started). Names compare the way Claude Code
//! compares them (every character outside `[A-Za-z0-9_-]` reads as `_`).
//!
//! The keys are read from:
//!
//! - the Prime Agent settings (`<agentDir>/settings.json`, and the
//!   project's `.prime/agent/settings.json`, which counts only once the
//!   workspace is trusted: the settings manager holds its other keys back
//!   until then);
//! - the choices the user made when asked, kept per workspace in
//!   `<agentDir>/mcp-project-choices.json` (outside the repository, so a
//!   cloned project cannot approve its own servers; Claude Code keeps them
//!   in `.claude/settings.local.json`);
//! - with the `claudeCodeSettings` opt-in, Claude Code's
//!   `~/.claude/settings.json`, and its project `.claude/settings.json` and
//!   `.claude/settings.local.json` once the workspace is explicitly trusted.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ClaudeCodeFiles, ProjectApproval};
use crate::settings::SettingsManager;

/// The choices store's file name inside the agent dir.
pub const PROJECT_CHOICES_FILE: &str = "mcp-project-choices.json";

const STORE_VERSION: u32 = 1;
const STALE_AFTER: Duration = Duration::from_secs(10);
const LOCK_ATTEMPTS: u32 = 10;

/// What the user answered for a project server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectChoice {
    /// Use this server.
    Approve,
    /// Use this server and every future one of this project.
    ApproveAll,
    /// Continue without it.
    Reject,
}

/// One workspace's recorded choices (Claude Code's key names).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectChoices {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_mcpjson_servers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_mcpjson_servers: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub enable_all_project_mcp_servers: bool,
}

impl ProjectChoices {
    fn is_empty(&self) -> bool {
        self == &ProjectChoices::default()
    }

    /// Record `choice` for `server`; `false` when nothing changed.
    fn record(&mut self, server: &str, choice: ProjectChoice) -> bool {
        let before = self.clone();
        match choice {
            ProjectChoice::Approve | ProjectChoice::ApproveAll => {
                self.disabled_mcpjson_servers
                    .retain(|name| !same_server_name(name, server));
                if !self
                    .enabled_mcpjson_servers
                    .iter()
                    .any(|name| same_server_name(name, server))
                {
                    self.enabled_mcpjson_servers.push(server.to_string());
                }
                if choice == ProjectChoice::ApproveAll {
                    self.enable_all_project_mcp_servers = true;
                }
            }
            ProjectChoice::Reject => {
                self.enabled_mcpjson_servers
                    .retain(|name| !same_server_name(name, server));
                if !self
                    .disabled_mcpjson_servers
                    .iter()
                    .any(|name| same_server_name(name, server))
                {
                    self.disabled_mcpjson_servers.push(server.to_string());
                }
            }
        }
        *self != before
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ChoicesDocument {
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, ProjectChoices>,
}

/// A server name as Claude Code compares it.
fn normalized(name: &str) -> String {
    name.chars()
        .map(|char| {
            if char.is_ascii_alphanumeric() || char == '_' || char == '-' {
                char
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether two server names are the same server to the approval lists.
#[must_use]
pub fn same_server_name(left: &str, right: &str) -> bool {
    normalized(left) == normalized(right)
}

fn store_path(agent_dir: &Path) -> std::path::PathBuf {
    agent_dir.join(PROJECT_CHOICES_FILE)
}

fn workspace_entry(workspace: &Path) -> String {
    workspace.display().to_string()
}

fn read_document(agent_dir: &Path) -> Result<ChoicesDocument> {
    let path = store_path(agent_dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ChoicesDocument {
                version: STORE_VERSION,
                projects: BTreeMap::new(),
            });
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let document: ChoicesDocument =
        serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))?;
    if document.version != STORE_VERSION {
        return Err(anyhow!(
            "{}: unsupported version {}",
            path.display(),
            document.version
        ));
    }
    Ok(document)
}

/// The recorded choices of `workspace`; a missing or damaged store reads
/// as none (nothing approved).
#[must_use]
pub fn read_project_choices(agent_dir: &Path, workspace: &Path) -> ProjectChoices {
    read_document(agent_dir)
        .ok()
        .and_then(|mut document| document.projects.remove(&workspace_entry(workspace)))
        .unwrap_or_default()
}

/// Read-modify-write the store under its lock; `change` returns whether it
/// changed anything. A damaged store is never overwritten.
fn update(
    agent_dir: &Path,
    workspace: &Path,
    change: impl FnOnce(&mut ProjectChoices) -> bool,
) -> Result<bool> {
    std::fs::create_dir_all(agent_dir)
        .with_context(|| format!("create {}", agent_dir.display()))?;
    let path = store_path(agent_dir);
    let mut attempts = 0;
    let _guard = loop {
        attempts += 1;
        match crate::platform::LockDir::acquire(&path, STALE_AFTER) {
            Ok(guard) => break guard,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && attempts < LOCK_ATTEMPTS =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error).with_context(|| format!("lock {}", path.display())),
        }
    };
    let mut document = read_document(agent_dir).with_context(|| {
        format!(
            "{} is unreadable; fix or remove it before changing MCP server choices",
            path.display()
        )
    })?;
    let key = workspace_entry(workspace);
    let mut choices = document.projects.remove(&key).unwrap_or_default();
    let changed = change(&mut choices);
    if !choices.is_empty() {
        document.projects.insert(key, choices);
    }
    if !changed {
        return Ok(false);
    }
    document.version = STORE_VERSION;
    let content = serde_json::to_string_pretty(&document)?;
    crate::settings::storage::atomic_write(&path, &format!("{content}\n"))?;
    Ok(true)
}

/// Record the user's answer for a project server of `workspace` (the
/// canonical workspace path, [`crate::workspace_trust::workspace_key`]).
///
/// # Errors
///
/// Returns an error when the store cannot be locked, parsed, or written.
pub fn record_project_choice(
    agent_dir: &Path,
    workspace: &Path,
    server: &str,
    choice: ProjectChoice,
) -> Result<()> {
    update(agent_dir, workspace, |choices| {
        choices.record(server, choice)
    })
    .map(|_| ())
}

/// Forget every recorded choice of `workspace` (Claude Code's `mcp
/// reset-project-choices`); `false` when there were none.
///
/// # Errors
///
/// Returns an error when the store cannot be locked, parsed, or written.
pub fn reset_project_choices(agent_dir: &Path, workspace: &Path) -> Result<bool> {
    update(agent_dir, workspace, |choices| {
        let had = !choices.is_empty();
        *choices = ProjectChoices::default();
        had
    })
}

/// The approval lists in force for one workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Approvals {
    pub(super) enable_all: bool,
    pub(super) enabled: Vec<String>,
    pub(super) disabled: Vec<String>,
}

impl Approvals {
    fn add_choices(&mut self, choices: &ProjectChoices) {
        self.enable_all |= choices.enable_all_project_mcp_servers;
        self.enabled
            .extend(choices.enabled_mcpjson_servers.iter().cloned());
        self.disabled
            .extend(choices.disabled_mcpjson_servers.iter().cloned());
    }

    /// The approval keys of a settings document (wrong-typed keys and
    /// entries are ignored).
    fn add_document(&mut self, document: &serde_json::Map<String, Value>) {
        let names = |key: &str| -> Vec<String> {
            document
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        };
        self.add_choices(&ProjectChoices {
            enabled_mcpjson_servers: names("enabledMcpjsonServers"),
            disabled_mcpjson_servers: names("disabledMcpjsonServers"),
            enable_all_project_mcp_servers: document
                .get("enableAllProjectMcpServers")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }

    fn add_file(&mut self, path: &Path) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        if let Ok(Value::Object(document)) = serde_json::from_str::<Value>(&content) {
            self.add_document(&document);
        }
    }

    /// Collect the lists of `workspace` (see the module docs).
    pub(super) fn collect(
        settings: &SettingsManager,
        agent_dir: &Path,
        cwd: &Path,
        workspace: &Path,
        claude_code: Option<&ClaudeCodeFiles>,
    ) -> Self {
        let mut approvals = Approvals::default();
        approvals.add_document(&settings.global_settings().extra);
        approvals.add_document(&settings.project_settings().extra);
        approvals.add_choices(&read_project_choices(agent_dir, workspace));
        if let Some(files) = claude_code {
            approvals.add_file(&files.user_settings);
            let explicitly_trusted = settings
                .workspace_trust()
                .is_some_and(|status| status.state == crate::workspace_trust::TrustState::Trusted);
            if explicitly_trusted {
                approvals.add_file(&cwd.join(".claude").join("settings.json"));
                approvals.add_file(&cwd.join(".claude").join("settings.local.json"));
            }
        }
        approvals
    }

    /// The approval of the project server `name`.
    pub(super) fn decide(&self, name: &str) -> ProjectApproval {
        if self
            .disabled
            .iter()
            .any(|server| same_server_name(server, name))
        {
            return ProjectApproval::Rejected;
        }
        if self.enable_all
            || self
                .enabled
                .iter()
                .any(|server| same_server_name(server, name))
        {
            return ProjectApproval::Approved;
        }
        ProjectApproval::Pending
    }
}
