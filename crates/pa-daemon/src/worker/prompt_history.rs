//! Per-project prompt history: the daemon half of up-arrow recall that
//! survives restarts and resume (capability `prompt_history`).
//!
//! The daemon is the sole writer of `<agent dir>/projects/<project>/
//! history.jsonl` (`pa_core::prompt_history`). `append_prompt_history` records
//! one submitted prompt — the client sends the text exactly as the user typed
//! it, across every input mode (plain, steer, follow-up, slash, `!` bash) —
//! and `get_prompt_history` serves the recall list (newest-first, deduped,
//! capped) to a client on attach. Both are no-ops when the environment turns
//! the feature off, matching Claude Code's `CLAUDE_CODE_SKIP_PROMPT_HISTORY`.

use std::path::Path;

use pa_core::prompt_history::{self, HistoryEntry};
use pa_types::sync::MutexExt;
use serde_json::{Value, json};

use super::{Worker, response_success};
use crate::protocol::DaemonResponse;

impl Worker {
    /// Record one submitted prompt in the project history (fire-and-forget).
    pub(crate) fn handle_append_prompt_history(&self, payload: &Value) -> DaemonResponse {
        const COMMAND: &str = "append_prompt_history";
        let display = payload
            .get("display")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if display.trim().is_empty() || prompt_history::disabled_by_env() {
            return response_success(None, COMMAND, None);
        }
        let cwd = self.core.lock_or_recover().cwd.clone();
        if cwd.is_empty() {
            return response_success(None, COMMAND, None);
        }
        let entry = HistoryEntry::now(
            display,
            Some(cwd.clone()),
            Some(self.config.active_session_id.clone()),
        );
        prompt_history::append(&self.config.agent_dir, Path::new(&cwd), &entry);
        response_success(None, COMMAND, None)
    }

    /// Serve the project's prompt history for recall on attach.
    pub(crate) fn handle_get_prompt_history(&self, payload: &Value) -> DaemonResponse {
        const COMMAND: &str = "get_prompt_history";
        let limit = payload
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(prompt_history::MAX_RECALL, |limit| {
                usize::try_from(limit).unwrap_or(prompt_history::MAX_RECALL)
            })
            .min(prompt_history::MAX_RECALL);
        let cwd = self.core.lock_or_recover().cwd.clone();
        if cwd.is_empty() || prompt_history::disabled_by_env() {
            return response_success(None, COMMAND, Some(json!({ "entries": [] })));
        }
        let entries = prompt_history::load(&self.config.agent_dir, Path::new(&cwd), limit);
        response_success(None, COMMAND, Some(json!({ "entries": entries })))
    }
}
