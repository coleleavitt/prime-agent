//! Workspace trust: the gate on project-scope configuration that can run
//! code or change the agent's instructions.
//!
//! A cloned repository can carry `.prime/agent/settings.json` (shell,
//! npm, MCP and package keys), `SYSTEM.md` / `APPEND_SYSTEM.md`, and
//! prompt templates. None of it applies until the user trusts the
//! workspace. The decision is recorded in `<agentDir>/trusted-workspaces.json`,
//! keyed by the workspace's canonical path and pinned to a content hash
//! of the gated files, so a change to them asks again.
//!
//! [`evaluate`] is the single read every entry point goes through
//! (`SettingsManager::create` consults it), so the interactive client,
//! print/json/rpc/acp modes, and daemon workers enforce one answer. Only
//! the composition root asks the user; a worker never prompts.

mod fingerprint;
mod store;

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Result;

pub use store::{TrustDecision, TrustRecord, TRUST_STORE_FILE};

/// Project settings keys that still apply in an untrusted workspace.
///
/// Each one only changes presentation or turn behaviour within providers
/// and models the user already configured: none names a command, a path,
/// a package, a server, or a prompt, and none raises an autonomy or cost
/// limit. Every other key (including unknown ones) is ignored until the
/// workspace is trusted, so a key added later fails closed.
pub const UNTRUSTED_PROJECT_SETTINGS_KEYS: &[&str] = &[
    "theme",
    "defaultProvider",
    "defaultModel",
    "defaultThinkingLevel",
    "enabledModels",
    "thinkingBudgets",
    "steeringMode",
    "followUpMode",
    "transport",
    "compaction",
    "branchSummary",
    "retry",
    "terminal",
    "images",
    "treeFilterMode",
    "chatDetail",
    "editorPaddingX",
    "autocompleteMaxVisible",
    "showHardwareCursor",
    "markdown",
    "warnings",
    "quietStartup",
    "requestTiming",
    "enableSkillCommands",
];

/// Whether a project settings key applies in an untrusted workspace.
#[must_use]
pub fn is_untrusted_safe_settings_key(key: &str) -> bool {
    UNTRUSTED_PROJECT_SETTINGS_KEYS.contains(&key)
}

/// One piece of project configuration the gate holds back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatedItem {
    /// `.prime/agent/settings.json` keys outside the safe list.
    SettingsKeys(Vec<String>),
    /// `.prime/agent/SYSTEM.md`: replaces the system prompt.
    SystemPrompt,
    /// `.prime/agent/APPEND_SYSTEM.md`: appends to the system prompt.
    AppendSystemPrompt,
    /// `.prime/agent/prompts/`: prompt templates.
    PromptTemplates,
}

impl fmt::Display for GatedItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let config_dir = crate::settings::CONFIG_DIR_NAME;
        match self {
            GatedItem::SettingsKeys(keys) => {
                write!(f, "{config_dir}/settings.json keys: {}", keys.join(", "))
            }
            GatedItem::SystemPrompt => write!(f, "{config_dir}/SYSTEM.md"),
            GatedItem::AppendSystemPrompt => write!(f, "{config_dir}/APPEND_SYSTEM.md"),
            GatedItem::PromptTemplates => write!(f, "{config_dir}/prompts/ (prompt templates)"),
        }
    }
}

/// The trust answer for one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustState {
    /// Nothing to gate: the workspace carries no gated project
    /// configuration, or its project config dir is the agent dir itself.
    NotRequired,
    /// Trusted, and the gated content still matches the recorded hash.
    Trusted,
    /// The user declined this exact content.
    Denied,
    /// No decision recorded for this workspace.
    Unknown,
    /// A decision exists, but the gated content changed since.
    Changed,
}

/// The evaluated trust of one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTrustStatus {
    /// The canonical workspace path (the store key).
    pub workspace: PathBuf,
    pub state: TrustState,
    /// What the gate holds back while untrusted (empty when nothing is gated).
    pub gated: Vec<GatedItem>,
}

impl WorkspaceTrustStatus {
    /// Project configuration applies.
    #[must_use]
    pub fn is_trusted(&self) -> bool {
        match self.state {
            TrustState::NotRequired | TrustState::Trusted => true,
            TrustState::Denied | TrustState::Unknown | TrustState::Changed => false,
        }
    }

    /// The user has not answered for the current content: an interactive
    /// client asks; a headless one prints [`Self::notice`].
    #[must_use]
    pub fn needs_decision(&self) -> bool {
        match self.state {
            TrustState::Unknown | TrustState::Changed => true,
            TrustState::NotRequired | TrustState::Trusted | TrustState::Denied => false,
        }
    }

    /// The one-paragraph notice a mode prints when it skipped project
    /// configuration; `None` when nothing was skipped.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        let reason = match self.state {
            TrustState::NotRequired | TrustState::Trusted => return None,
            TrustState::Unknown => "is not trusted",
            TrustState::Denied => "was not trusted when you were asked",
            TrustState::Changed => "changed its project configuration since you last decided",
        };
        let items = self
            .gated
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "Workspace {} {reason}, so its project configuration was not loaded ({items}). \
             Run `prime-agent trust` in this directory, or pass --trust-workspace, to load it.",
            self.workspace.display()
        ))
    }
}

/// The store file for an agent dir.
#[must_use]
pub fn store_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(TRUST_STORE_FILE)
}

/// The canonical store key for a workspace (the path as given when it
/// cannot be canonicalized).
#[must_use]
pub fn workspace_key(cwd: &Path) -> PathBuf {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// Evaluate a workspace's trust against the store. Never prompts and never
/// writes; an unreadable or corrupt store reads as "no decision" (closed).
#[must_use]
pub fn evaluate(cwd: &Path, agent_dir: &Path) -> WorkspaceTrustStatus {
    let workspace = workspace_key(cwd);
    let inputs = fingerprint::TrustInputs::collect(cwd, agent_dir);
    if inputs.is_empty() {
        return WorkspaceTrustStatus {
            workspace,
            state: TrustState::NotRequired,
            gated: Vec::new(),
        };
    }
    let gated = inputs.gated();
    let state = match store::read_record(agent_dir, &workspace) {
        None => TrustState::Unknown,
        Some(record) if record.content_hash != inputs.digest() => TrustState::Changed,
        Some(record) => match record.decision {
            TrustDecision::Trusted => TrustState::Trusted,
            TrustDecision::Denied => TrustState::Denied,
        },
    };
    WorkspaceTrustStatus {
        workspace,
        state,
        gated,
    }
}

/// Record a decision for the workspace's current gated content.
///
/// # Errors
///
/// Returns an error when the store cannot be locked, parsed, or written.
pub fn record(
    cwd: &Path,
    agent_dir: &Path,
    decision: TrustDecision,
) -> Result<WorkspaceTrustStatus> {
    let workspace = workspace_key(cwd);
    let digest = fingerprint::TrustInputs::collect(cwd, agent_dir).digest();
    store::update(agent_dir, |records| {
        records.insert(
            workspace.display().to_string(),
            TrustRecord {
                decision,
                content_hash: digest.clone(),
                decided_at_ms: store::now_ms(),
            },
        );
        true
    })?;
    Ok(evaluate(cwd, agent_dir))
}

/// Re-pin a trusted workspace's record to its current content: the
/// product's own writes to gated project settings (package installs,
/// resource toggles) must not revoke the trust the user gave. A workspace
/// without a `trusted` record is left alone.
///
/// # Errors
///
/// Returns an error when the store cannot be locked, parsed, or written.
pub fn refresh_trusted(cwd: &Path, agent_dir: &Path) -> Result<()> {
    let key = workspace_key(cwd).display().to_string();
    let digest = fingerprint::TrustInputs::collect(cwd, agent_dir).digest();
    store::update(agent_dir, |records| match records.get_mut(&key) {
        Some(record) if record.decision == TrustDecision::Trusted => {
            if record.content_hash == digest {
                return false;
            }
            record.content_hash.clone_from(&digest);
            record.decided_at_ms = store::now_ms();
            true
        }
        Some(_) | None => false,
    })
}

/// Every recorded decision, keyed by canonical workspace path.
///
/// # Errors
///
/// Returns an error when the store exists but cannot be read or parsed.
pub fn list(agent_dir: &Path) -> Result<Vec<(String, TrustRecord)>> {
    Ok(store::read_all(agent_dir)?.into_iter().collect())
}

#[cfg(test)]
mod tests;
