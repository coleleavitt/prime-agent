//! `/cwd` (upstream #2528): retarget the live session's working directory.
//!
//! The change lands on the session BRANCH (a `session_cwd_state` custom
//! entry), so a resume restarts in the recorded directory and a tree move
//! follows the directory recorded on the target branch. One resolution
//! rule serves the create and every tree move: an explicit cwd override
//! (the create carried its own `cwd` for a saved session) pins the run;
//! otherwise the newest still-existing `session_cwd_state` entry on the
//! active branch wins, else the header cwd when it still exists, else the
//! cwd stays. The running Python kernel, `!` commands, and subagents
//! spawned afterwards follow at once; the model learns of the move through
//! a `[cwd-changed]` notice delivered with the next turn. The system prompt
//! and project context are not reloaded.

use std::path::{Component, Path, PathBuf};

use pa_types::sync::MutexExt;
use serde_json::{json, Value};

use super::{response_failure, response_success, SessionFile, Worker};
use crate::protocol::DaemonResponse;

/// The branch entry recording one `/cwd` move (TS `session_cwd_state`).
pub(crate) const SESSION_CWD_STATE_CUSTOM_TYPE: &str = "session_cwd_state";
/// The displayed next-turn notice of one move (TS `session_cwd_changed`).
pub(crate) const SESSION_CWD_CHANGED_CUSTOM_TYPE: &str = "session_cwd_changed";
/// TS's refusal while a turn runs.
pub(crate) const CWD_BUSY_ERROR: &str =
    "Cannot change the working directory while the agent is running.";

/// A directory the process can stat (missing, non-directory, and
/// inaccessible paths all answer `false`).
pub(crate) fn is_existing_directory(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// The resolution rule's branch half: the newest still-existing recorded
/// directory on the active branch, else the header cwd when it still
/// exists; `None` keeps the current cwd.
pub(crate) fn branch_cwd(store: &SessionFile) -> Option<String> {
    store
        .branch()
        .iter()
        .rev()
        .filter(|entry| {
            entry.type_ == "custom"
                && entry.fields.get("customType").and_then(Value::as_str)
                    == Some(SESSION_CWD_STATE_CUSTOM_TYPE)
        })
        .filter_map(|entry| {
            entry
                .fields
                .get("data")
                .and_then(|data| data.get("cwd"))
                .and_then(Value::as_str)
        })
        .find(|cwd| is_existing_directory(Path::new(cwd)))
        .map(str::to_string)
        .or_else(|| {
            let header = &store.header.cwd;
            (!header.is_empty() && is_existing_directory(Path::new(header))).then(|| header.clone())
        })
}

/// TS `resolve(cwd, expandTildePath(input))`: `~` expands, a relative path
/// joins the current cwd, and `.`/`..` fold lexically.
pub(crate) fn resolve_cwd_input(input: &str, current: &Path) -> anyhow::Result<PathBuf> {
    let expanded = crate::paths::expand_tilde(input.trim())?;
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        current.join(expanded)
    };
    let mut resolved = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    Ok(resolved)
}

/// TS `cwdChangedNotice`.
pub(crate) fn cwd_changed_notice(previous: &str, cwd: &str) -> String {
    [
        "[cwd-changed]".to_string(),
        String::new(),
        format!("The user ran /cwd. This session's working directory is now {cwd} (previously {previous}); it stays so for the rest of the session, including after a resume."),
        format!("The Python kernel's working directory is now {cwd} (os.getcwd()): bash() and relative paths resolve there, and new subagents start there. The \"Working directory\" line in the system prompt is from session start and no longer applies. Do not chdir back unless the user asks."),
    ]
    .join("\n")
}

/// TS `cwdKernelStaleNotice`: a tree move whose kernel refused the chdir.
pub(crate) fn cwd_kernel_stale_notice(previous: &str, cwd: &str, error: &str) -> String {
    let literal = serde_json::to_string(cwd).unwrap_or_default();
    [
        "[cwd-changed]".to_string(),
        String::new(),
        format!("Tree navigation moved this session's working directory to {cwd} (previously {previous}), but the running Python kernel could not change directory: {error}."),
        format!("bash() and relative paths in the kernel still resolve in {previous} until you run __import__(\"os\").chdir({literal}) or the kernel restarts (it starts in {cwd}). New subagents and ! commands already use {cwd}."),
    ]
    .join("\n")
}

fn notice_row(content: &str, cwd: &str, previous: &str) -> Value {
    json!({
        "role": "custom",
        "customType": SESSION_CWD_CHANGED_CUSTOM_TYPE,
        "content": content,
        "display": true,
        "details": { "cwd": cwd, "previousCwd": previous },
        "timestamp": crate::util::now_ms(),
    })
}

fn is_cwd_notice(row: &Value) -> bool {
    row.get("customType").and_then(Value::as_str) == Some(SESSION_CWD_CHANGED_CUSTOM_TYPE)
}

impl Worker {
    /// `set_cwd { cwd }`: refused while a turn runs; the path resolves
    /// against the current cwd and must be a directory; the kernel moves
    /// first (a refused chdir records nothing), then the branch records the
    /// move (a failed record moves the kernel back).
    pub(crate) async fn handle_set_cwd(&self, payload: &Value) -> DaemonResponse {
        const COMMAND: &str = "set_cwd";
        if let Err(response) = self.require_created(COMMAND) {
            return response;
        }
        let input = payload.get("cwd").and_then(Value::as_str).unwrap_or("");
        if input.trim().is_empty() {
            return response_failure(None, COMMAND, "Usage: /cwd <path>", None);
        }
        let previous = {
            let core = self.core.lock_or_recover();
            if core.busy {
                return response_failure(None, COMMAND, CWD_BUSY_ERROR, None);
            }
            core.cwd.clone()
        };
        let resolved = match resolve_cwd_input(input, Path::new(&previous)) {
            Ok(resolved) => resolved,
            Err(error) => return response_failure(None, COMMAND, &error.to_string(), None),
        };
        let cwd = resolved.display().to_string();
        if !is_existing_directory(&resolved) {
            return response_failure(None, COMMAND, &format!("Not a directory: {cwd}"), None);
        }
        if cwd != previous {
            if let Err(error) = self.engine.retarget_kernel_cwd(resolved.clone()).await {
                let _ = self
                    .engine
                    .retarget_kernel_cwd(PathBuf::from(&previous))
                    .await;
                return response_failure(None, COMMAND, &format!("{error:#}"), None);
            }
            let recorded = {
                let mut core = self.core.lock_or_recover();
                let recorded = match core.store.as_mut() {
                    Some(store) => store
                        .persist_entry(
                            "custom",
                            json!({
                                "customType": SESSION_CWD_STATE_CUSTOM_TYPE,
                                "data": { "cwd": cwd },
                            }),
                        )
                        .map(|_| ()),
                    None => Ok(()),
                };
                if recorded.is_ok() {
                    core.cwd.clone_from(&cwd);
                    core.pending_next_turn.retain(|row| !is_cwd_notice(row));
                    core.pending_next_turn.push(notice_row(
                        &cwd_changed_notice(&previous, &cwd),
                        &cwd,
                        &previous,
                    ));
                }
                recorded
            };
            if let Err(error) = recorded {
                let _ = self
                    .engine
                    .retarget_kernel_cwd(PathBuf::from(&previous))
                    .await;
                return response_failure(None, COMMAND, &error.to_string(), None);
            }
            self.engine.set_cwd(resolved.clone());
            self.engine.note_cwd_changed();
            self.publish_cwd(&cwd);
        }
        // Project-scope configuration in an untrusted directory stays
        // ignored (every settings read goes through the trust gate); the
        // client tells the user so.
        let trust = pa_core::workspace_trust::evaluate(&resolved, &self.config.agent_dir);
        response_success(
            None,
            COMMAND,
            Some(json!({
                "cwd": cwd,
                "workspaceTrusted": !trust.needs_decision(),
            })),
        )
    }

    /// After a tree move (TS `_reloadCwdFromBranch`): queued `[cwd-changed]`
    /// notices drop, and unless the run is pinned the session follows the
    /// target branch's recorded directory. A kernel that refuses the chdir
    /// is reported (a displayed notice for the user and the model), never
    /// rolled back.
    pub(crate) async fn follow_branch_cwd(&self) {
        let (previous, target) = {
            let mut core = self.core.lock_or_recover();
            core.pending_next_turn.retain(|row| !is_cwd_notice(row));
            if core.cwd_override {
                return;
            }
            let Some(target) = core.store.as_ref().and_then(branch_cwd) else {
                return;
            };
            if target == core.cwd {
                return;
            }
            let previous = std::mem::replace(&mut core.cwd, target.clone());
            (previous, target)
        };
        self.engine.set_cwd(PathBuf::from(&target));
        self.publish_cwd(&target);
        if let Err(error) = self
            .engine
            .retarget_kernel_cwd(PathBuf::from(&target))
            .await
        {
            let row = notice_row(
                &cwd_kernel_stale_notice(&previous, &target, &format!("{error:#}")),
                &target,
                &previous,
            );
            self.core
                .lock_or_recover()
                .pending_next_turn
                .push(row.clone());
            self.emit_custom_row(&row);
        }
    }

    /// Tell attached clients (footer, title, autocomplete) and the roster
    /// about the moved cwd.
    fn publish_cwd(&self, cwd: &str) {
        self.emit_worker_event(json!({ "type": "cwd_changed", "cwd": cwd }));
        let summary = {
            let (core, inputs) = self.summary_inputs();
            self.summary_locked(&core, inputs)
        };
        if let Ok(summary) = serde_json::to_value(&summary) {
            self.engine.set_session_summary(summary);
        }
    }
}

#[cfg(test)]
mod tests;
