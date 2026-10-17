//! `/rewind` (Claude Code's checkpoints, Esc Esc): the worker-side handlers
//! for `get_rewind_checkpoints` and `rewind` (capability `rewind`).
//!
//! A checkpoint is a user prompt on the active branch. Its conversation side
//! is the `navigate_tree` move onto the prompt (the leaf lands on its parent,
//! the prompt's text returns to the editor; "Summarize from here" summarizes
//! the abandoned branch). Its code side is the session's file checkpoints
//! ([`pa_core::file_history`]) at the time the prompt's entry was written:
//! every file the kernel's edit helpers wrote since goes back to its state
//! then. Files changed by `bash()` or by hand are not tracked, and one
//! changed outside the agent since its last edit is left alone unless the
//! client forces it.

use pa_core::file_history::{FileHistory, PlannedAction, RestorePlan, RestoreReport};
use pa_types::sync::MutexExt;
use serde_json::{Value, json};

use crate::protocol::{DaemonResponse, response_failure, response_success};
use crate::session_store::SessionFile;
use crate::session_tree::{self, BranchUserPrompt};
use crate::worker::Worker;

const GET_CHECKPOINTS: &str = "get_rewind_checkpoints";
const REWIND: &str = "rewind";

/// The session's file history, when the session persists and anything was
/// ever recorded for it.
fn session_history(store: &SessionFile) -> Option<FileHistory> {
    if store.path.as_os_str().is_empty() {
        return None;
    }
    let dir = pa_core::session_engine::harness_digest::session_artifact_dir_for_log(&store.path)?;
    let history = FileHistory::for_artifact_dir(&dir);
    history.exists().then_some(history)
}

/// The picker's rows: each prompt with its code summary (`code` only when
/// the session has file checkpoints).
pub(crate) fn checkpoints_json(
    prompts: &[BranchUserPrompt],
    plans: Option<&[RestorePlan]>,
) -> Vec<Value> {
    prompts
        .iter()
        .enumerate()
        .map(|(index, prompt)| {
            let mut row = json!({
                "entryId": prompt.entry_id,
                "text": prompt.text,
                "timestamp": prompt.timestamp_ms,
            });
            if let Some(plan) = plans.and_then(|plans| plans.get(index)) {
                row["code"] = plan_json(plan);
            }
            row
        })
        .collect()
}

/// One restore plan on the wire: the files a code restore would change
/// (restorable ones in `filesChanged`), its line stats, the files changed
/// outside the agent since (left alone unless forced), and the ones it
/// cannot restore.
pub(crate) fn plan_json(plan: &RestorePlan) -> Value {
    let mut files_changed = Vec::new();
    let mut externally_modified = Vec::new();
    let mut unrestorable = Vec::new();
    let (mut insertions, mut deletions) = (0, 0);
    for file in &plan.files {
        match &file.action {
            PlannedAction::Restore | PlannedAction::Delete => {
                if file.externally_modified {
                    externally_modified.push(json!(file.path));
                } else {
                    files_changed.push(json!(file.path));
                    insertions += file.insertions;
                    deletions += file.deletions;
                }
            }
            PlannedAction::Unrestorable { reason } => {
                unrestorable.push(json!({ "path": file.path, "reason": reason }));
            }
        }
    }
    json!({
        "filesChanged": files_changed,
        "insertions": insertions,
        "deletions": deletions,
        "externallyModified": externally_modified,
        "unrestorable": unrestorable,
    })
}

/// A code restore's outcome on the wire.
pub(crate) fn report_json(report: &RestoreReport) -> Value {
    json!({
        "restored": report.restored,
        "deleted": report.deleted,
        "skipped": report
            .skipped
            .iter()
            .map(|skipped| json!({ "path": skipped.path, "reason": skipped.reason }))
            .collect::<Vec<_>>(),
    })
}

/// What one `rewind` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RewindRequest {
    entry_id: String,
    restore_code: bool,
    restore_conversation: bool,
    summarize: bool,
    custom_instructions: Option<String>,
    force: bool,
}

impl RewindRequest {
    fn parse(payload: &Value) -> Result<Self, String> {
        let flag = |key: &str| payload.get(key).and_then(Value::as_bool) == Some(true);
        let entry_id = payload
            .get("entryId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("rewind needs an entryId")?
            .to_string();
        let request = Self {
            entry_id,
            restore_code: flag("restoreCode"),
            restore_conversation: flag("restoreConversation"),
            summarize: flag("summarize"),
            custom_instructions: payload
                .get("customInstructions")
                .and_then(Value::as_str)
                .map(str::to_string),
            force: flag("force"),
        };
        if !(request.restore_code || request.restore_conversation || request.summarize) {
            return Err("rewind needs restoreCode, restoreConversation or summarize".to_string());
        }
        Ok(request)
    }
}

impl Worker {
    /// `get_rewind_checkpoints`: the active branch's user prompts, oldest
    /// first, each with what a code restore to before it would change.
    pub(crate) async fn handle_get_rewind_checkpoints(&self) -> DaemonResponse {
        let (prompts, history) = {
            let core = self.core.lock_or_recover();
            let Some(store) = core.store.as_ref() else {
                return response_failure(
                    None,
                    GET_CHECKPOINTS,
                    "Session is still initializing",
                    None,
                );
            };
            (
                session_tree::branch_user_prompts(store),
                session_history(store),
            )
        };
        let code_restore = history.is_some();
        let points: Vec<u64> = prompts.iter().map(|prompt| prompt.timestamp_ms).collect();
        let plans = match history {
            Some(history) => {
                match tokio::task::spawn_blocking(move || history.plans(&points)).await {
                    Ok(Ok(plans)) => Some(plans),
                    Ok(Err(error)) => {
                        return response_failure(
                            None,
                            GET_CHECKPOINTS,
                            &format!("file checkpoints unreadable: {error}"),
                            None,
                        );
                    }
                    Err(join) => {
                        return response_failure(None, GET_CHECKPOINTS, &join.to_string(), None);
                    }
                }
            }
            None => None,
        };
        response_success(
            None,
            GET_CHECKPOINTS,
            Some(json!({
                "codeRestore": code_restore,
                "checkpoints": checkpoints_json(&prompts, plans.as_deref()),
            })),
        )
    }

    /// `rewind`: settle the running turn, restore the code (first, like
    /// Claude Code), then move the conversation. A failed code restore still
    /// moves the conversation when asked to; the response carries both.
    pub(crate) async fn handle_rewind(&self, payload: &Value) -> DaemonResponse {
        let request = match RewindRequest::parse(payload) {
            Ok(request) => request,
            Err(error) => return response_failure(None, REWIND, &error, None),
        };
        let (prompt, history) = {
            let core = self.core.lock_or_recover();
            let Some(store) = core.store.as_ref() else {
                return response_failure(None, REWIND, "Session is still initializing", None);
            };
            let prompt = session_tree::branch_user_prompts(store)
                .into_iter()
                .find(|prompt| prompt.entry_id == request.entry_id);
            (prompt, session_history(store))
        };
        let Some(prompt) = prompt else {
            return response_failure(
                None,
                REWIND,
                &format!(
                    "Entry {} is not a user prompt on the active branch",
                    request.entry_id
                ),
                None,
            );
        };
        // A restore must not race the agent's own writes: the running turn
        // stops first, like a tree move.
        self.tree_navigation.wait_turn_end().await;

        let mut data = json!({});
        if request.restore_code {
            let restored = match history {
                Some(history) => {
                    let force = request.force;
                    let at_ms = prompt.timestamp_ms;
                    tokio::task::spawn_blocking(move || {
                        history.restore(at_ms, force, pa_core::file_history::now_ms())
                    })
                    .await
                    .map_err(|join| join.to_string())
                    .and_then(|result| result.map_err(|error| error.to_string()))
                }
                None => Err("No file checkpoints for this session".to_string()),
            };
            match restored {
                Ok(report) => data["code"] = report_json(&report),
                Err(error) if !(request.restore_conversation || request.summarize) => {
                    return response_failure(
                        None,
                        REWIND,
                        &format!("Failed to restore the code: {error}"),
                        None,
                    );
                }
                Err(error) => data["codeError"] = json!(error),
            }
        }
        if request.restore_conversation || request.summarize {
            let mut navigation = json!({
                "targetId": request.entry_id,
                "summarize": request.summarize,
            });
            if let Some(instructions) = &request.custom_instructions {
                navigation["customInstructions"] = json!(instructions);
            }
            let moved = self.handle_navigate_tree(&navigation).await;
            if !moved.success {
                let error = moved.error.unwrap_or_default();
                return response_failure(
                    None,
                    REWIND,
                    &format!("Failed to restore the conversation: {error}"),
                    None,
                );
            }
            data["conversation"] = moved.data.unwrap_or_else(|| json!({}));
        }
        response_success(None, REWIND, Some(data))
    }
}

#[cfg(test)]
mod tests;
