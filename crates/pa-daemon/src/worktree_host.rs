//! Mid-session worktree isolation (Claude Code's `EnterWorktree` /
//! `ExitWorktree`): the kernel's `rlm.enter_worktree` and
//! `rlm.exit_worktree` send `worktree.enter` / `worktree.exit`, and the
//! session moves into or out of a managed worktree (`pa_core::worktree`).
//!
//! The kernel makes the move itself: it is inside the cell that awaits this
//! request, so the host cannot run a chdir cell. The host creates (or
//! removes) the worktree, records the move on the session branch through the
//! worker's [`WorktreeSession`] seam (a `session_cwd_state` entry, so a resume
//! restarts there, plus a `worktree_session_state` entry for the exit),
//! points later kernel starts and MCP stdio servers at it, and answers with
//! the directory the kernel then changes to. The system prompt is not
//! rebuilt: the reply tells the model where it now works.
//!
//! Leaving is never destructive by default: `remove` refuses a worktree the
//! session did not create, and one with uncommitted files or new commits
//! unless `discardChanges` is set (Claude Code's rule and wording).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_core::kernel::shared::{HostRequestHandlers, host_handler};
use pa_core::worktree::{CreateRequest, RemoveOutcome, Worktree, WorktreeHookContext};
use pa_types::sync::MutexExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent_engine::AgentSessionEngine;

/// The branch entry recording the session's worktree state.
pub(crate) const WORKTREE_SESSION_STATE_CUSTOM_TYPE: &str = "worktree_session_state";

/// The session's worktree state, as the branch records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSessionState {
    /// The session is working in the worktree (false once it left).
    pub active: bool,
    /// Where the session worked before it first entered a worktree.
    pub original_cwd: String,
    pub worktree: Worktree,
}

/// What the worker knows about the session right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSessionView {
    pub cwd: String,
    pub session_id: String,
    /// The newest recorded state on the active branch.
    pub state: Option<WorktreeSessionState>,
}

/// The worker's side of a worktree move.
pub trait WorktreeSession: Send + Sync {
    fn view(&self) -> WorktreeSessionView;

    /// Record a move to `cwd` with `state`: the branch entries, the
    /// session's cwd, the engine's, and the clients' `cwd_changed`.
    ///
    /// # Errors
    ///
    /// The branch entries cannot be persisted (nothing moved then).
    fn moved(&self, cwd: &str, state: &WorktreeSessionState) -> anyhow::Result<()>;
}

/// The newest `worktree_session_state` among `entries` (branch order).
pub(crate) fn latest_state<'a>(
    entries: impl DoubleEndedIterator<Item = &'a Value>,
) -> Option<WorktreeSessionState> {
    entries
        .rev()
        .filter(|fields| {
            fields.get("customType").and_then(Value::as_str)
                == Some(WORKTREE_SESSION_STATE_CUSTOM_TYPE)
        })
        .find_map(|fields| serde_json::from_value(fields.get("data")?.clone()).ok())
}

impl AgentSessionEngine {
    /// Wire the worker's worktree seam (once, before the first session
    /// build reads it).
    pub fn set_worktree_session(&self, seam: Arc<dyn WorktreeSession>) {
        *self.worktree_session.lock_or_recover() = Some(seam);
    }

    /// The `worktree.enter` / `worktree.exit` kernel host handlers;
    /// registered only when the worker wired its seam.
    pub(crate) fn register_worktree_host_handlers(&self, handlers: &mut HostRequestHandlers) {
        let Some(seam) = self.worktree_session.lock_or_recover().clone() else {
            return;
        };
        let context = WorktreeHostContext {
            seam,
            session: Arc::clone(&self.session),
            agent_dir: self.config.agent_dir.clone(),
        };
        let enter_context = context.clone();
        handlers.register(
            "worktree.enter",
            host_handler(move |payload| {
                let context = enter_context.clone();
                Box::pin(async move { context.enter(&payload.data).await })
            }),
        );
        handlers.register(
            "worktree.exit",
            host_handler(move |payload| {
                let context = context.clone();
                Box::pin(async move { context.exit(&payload.data).await })
            }),
        );
    }
}

type CoreSessionSlot =
    Arc<tokio::sync::Mutex<Option<Arc<pa_core::session_engine::engine::SessionEngine>>>>;

#[derive(Clone)]
struct WorktreeHostContext {
    seam: Arc<dyn WorktreeSession>,
    session: CoreSessionSlot,
    agent_dir: PathBuf,
}

impl WorktreeHostContext {
    fn hook_context(&self, view: &WorktreeSessionView) -> WorktreeHookContext {
        WorktreeHookContext {
            cwd: PathBuf::from(&view.cwd),
            agent_dir: self.agent_dir.clone(),
            session_id: Some(view.session_id.clone()),
        }
    }

    /// Record the move and point later kernel starts at `cwd`.
    async fn move_to(&self, cwd: &str, state: &WorktreeSessionState) -> anyhow::Result<()> {
        self.seam.moved(cwd, state)?;
        if let Some(core) = self.session.lock().await.clone() {
            core.follow_kernel_cwd(Path::new(cwd));
        }
        Ok(())
    }

    async fn enter(&self, data: &Value) -> anyhow::Result<Value> {
        let name = data.get("name").and_then(Value::as_str).map(str::to_string);
        let path = data.get("path").and_then(Value::as_str).map(PathBuf::from);
        if name.is_some() && path.is_some() {
            anyhow::bail!("Provide at most one of `name` or `path`, not both.");
        }
        let view = self.seam.view();
        let active = view.state.clone().filter(|state| state.active);
        if active.is_some() && path.is_none() {
            anyhow::bail!(
                "Already in a worktree session. Pass `path` to switch into another existing \
                 worktree, or call rlm.exit_worktree() to leave this one before creating a new \
                 worktree."
            );
        }
        let from = PathBuf::from(&view.cwd);
        let settings = pa_core::settings::SettingsManager::create(&from, &self.agent_dir);
        let request = CreateRequest {
            name,
            from: from.clone(),
            base: pa_core::worktree::configured_base(settings.settings()),
            hooks: Some(self.hook_context(&view)),
        };
        let worktree = tokio::task::spawn_blocking(move || match path {
            Some(path) => pa_core::worktree::open_existing(&from, &path),
            None => pa_core::worktree::create(&request),
        })
        .await??;
        let original_cwd = active.map_or_else(|| view.cwd.clone(), |state| state.original_cwd);
        let state = WorktreeSessionState {
            active: true,
            original_cwd,
            worktree: worktree.clone(),
        };
        let cwd = worktree.path.to_string_lossy().to_string();
        self.move_to(&cwd, &state).await?;
        let verb = if worktree.existed {
            "Entered existing"
        } else {
            "Created"
        };
        let branch = worktree
            .branch
            .as_deref()
            .map(|branch| format!(" on branch {branch}"))
            .unwrap_or_default();
        let copied = if worktree.copied.is_empty() {
            String::new()
        } else {
            format!(
                " Copied from .worktreeinclude: {}.",
                worktree.copied.join(", ")
            )
        };
        Ok(json!({
            "worktreePath": cwd,
            "worktreeBranch": worktree.branch,
            "existed": worktree.existed,
            "copied": worktree.copied,
            "message": format!(
                "{verb} worktree at {cwd}{branch}.{copied} The session is now working in the \
                 worktree (the system prompt's working directory no longer applies). Call \
                 rlm.exit_worktree(\"keep\" or \"remove\") to leave mid-session."
            ),
        }))
    }

    async fn exit(&self, data: &Value) -> anyhow::Result<Value> {
        let action = data.get("action").and_then(Value::as_str).unwrap_or("");
        if !matches!(action, "keep" | "remove") {
            anyhow::bail!("action must be \"keep\" or \"remove\"");
        }
        let discard = data
            .get("discardChanges")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let view = self.seam.view();
        let Some(state) = view.state.clone().filter(|state| state.active) else {
            return Ok(json!({
                "action": "none",
                "message": "No worktree session is active (rlm.enter_worktree was not called); \
                            nothing changed.",
            }));
        };
        let worktree = state.worktree.clone();
        let path = worktree.path.display().to_string();
        let branch = worktree
            .branch
            .as_deref()
            .map(|branch| format!(" on branch {branch}"))
            .unwrap_or_default();
        if action == "remove" && worktree.existed {
            anyhow::bail!(
                "This session is not the owner of the worktree at {path}: it entered an existing \
                 worktree, so this will not remove it. Use action \"keep\" to return to {}.",
                state.original_cwd
            );
        }
        let mut found = pa_core::worktree::WorktreeChanges::default();
        if action == "remove" && !discard {
            let probe = worktree.clone();
            let changes = tokio::task::spawn_blocking(move || {
                pa_core::worktree::changes(&probe.path, probe.base_commit.as_deref())
            })
            .await?;
            match changes {
                Err(_) => anyhow::bail!(
                    "Could not verify worktree state at {path}. Refusing to remove without \
                     explicit confirmation. Re-invoke with discard_changes=True to proceed, or \
                     use action \"keep\" to preserve the worktree."
                ),
                Ok(changes) if !changes.is_clean() => anyhow::bail!(
                    "Worktree has {}. Removing will discard this work permanently. Confirm with \
                     the user, then re-invoke with discard_changes=True, or use action \"keep\" \
                     to preserve the worktree.",
                    changes.describe(worktree.branch.as_deref())
                ),
                Ok(changes) => found = changes,
            }
        } else if action == "remove" {
            let probe = worktree.clone();
            if let Ok(Ok(changes)) = tokio::task::spawn_blocking(move || {
                pa_core::worktree::changes(&probe.path, probe.base_commit.as_deref())
            })
            .await
            {
                found = changes;
            }
        }
        // Back to where the session started (the repository root when that
        // directory is gone), before anything is removed.
        let target = [
            PathBuf::from(&state.original_cwd),
            worktree.repo_root.clone().unwrap_or_default(),
        ]
        .into_iter()
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| PathBuf::from(&view.cwd));
        let restored = target.to_string_lossy().to_string();
        let left = WorktreeSessionState {
            active: false,
            ..state.clone()
        };
        self.move_to(&restored, &left).await?;
        if action == "keep" {
            return Ok(json!({
                "action": "keep",
                "worktreePath": path,
                "worktreeBranch": worktree.branch,
                "restoredCwd": restored,
                "message": format!(
                    "Exited worktree. Your work is preserved at {path}{branch}. The session is \
                     back in {restored}."
                ),
            }));
        }
        let hooks = self.hook_context(&view);
        let removing = worktree.clone();
        let removed = tokio::task::spawn_blocking(move || {
            pa_core::worktree::remove(&removing, discard, Some(&hooks))
        })
        .await?;
        let message = match removed {
            Ok(RemoveOutcome::Removed { branch_deleted }) => {
                let mut discarded = Vec::new();
                if found.commits_ahead > 0 {
                    discarded.push(format!("{} commit(s)", found.commits_ahead));
                }
                if found.changed_files > 0 {
                    discarded.push(format!("{} uncommitted file(s)", found.changed_files));
                }
                let discarded = if discarded.is_empty() {
                    String::new()
                } else {
                    format!(" Discarded {}.", discarded.join(" and "))
                };
                let branch_note = match (&worktree.branch, branch_deleted) {
                    (Some(branch), false) => {
                        format!(" Branch {branch} was kept: its commits are on no other branch.")
                    }
                    _ => String::new(),
                };
                format!(
                    "Exited and removed worktree at {path}.{discarded}{branch_note} The session \
                     is back in {restored}."
                )
            }
            Ok(RemoveOutcome::Kept(changes)) => format!(
                "Exited worktree but kept it at {path}: it has {}. The session is back in \
                 {restored}.",
                changes.describe(worktree.branch.as_deref())
            ),
            Err(error) => format!(
                "Exited worktree but could not remove it; kept at {path} ({error}). The session \
                 is back in {restored}."
            ),
        };
        Ok(json!({
            "action": "remove",
            "worktreePath": path,
            "worktreeBranch": worktree.branch,
            "restoredCwd": restored,
            "message": message,
        }))
    }
}

#[cfg(test)]
mod tests;
