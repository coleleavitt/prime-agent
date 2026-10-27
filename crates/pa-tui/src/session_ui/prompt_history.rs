//! Per-project prompt history: the client half of up-arrow recall that
//! survives restarts and resume (daemon capability `prompt_history`).
//!
//! The daemon owns the store (one writer, `pa_core::prompt_history`). This
//! module does two things from the client:
//!
//! - [`SessionUi::record_prompt_history`]: fire-and-forget, called beside every
//!   `editor.add_to_history` so the exact text the user typed — across plain,
//!   steer, follow-up, slash and `!` bash submits — is persisted for future
//!   sessions.
//! - [`SessionUi::seed_prompt_history`]: on attach, loads the project's recall
//!   list and seeds the editor's up-arrow ring so a freshly opened or resumed
//!   session recalls prompts from every earlier session in the project.
//!
//! Both are no-ops when the daemon lacks the capability or the environment
//! turns the feature off (Claude Code's `CLAUDE_CODE_SKIP_PROMPT_HISTORY`).

use std::time::Duration;

use pa_types::daemon::DaemonCommand;
use serde_json::{Map, Value};

use super::{AgentView, SessionUi, UI_REQUEST_TIMEOUT_MS};

/// The daemon server capability behind `append_prompt_history` and
/// `get_prompt_history` (mirrors `pa_daemon::protocol::PROMPT_HISTORY_CAPABILITY`;
/// pa-tui is a wire client and does not link the daemon).
const PROMPT_HISTORY_CAPABILITY: &str = "prompt_history";

/// The environment switch that turns prompt history off (mirrors
/// `pa_core::prompt_history::DISABLE_ENV`).
const DISABLE_ENV: &str = "PRIME_AGENT_DISABLE_PROMPT_HISTORY";

fn disabled_by_env() -> bool {
    std::env::var(DISABLE_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

impl SessionUi {
    /// Whether prompt history is live for this session (daemon supports it and
    /// the environment has not turned it off).
    fn prompt_history_active(&self) -> bool {
        !disabled_by_env()
            && self
                .client
                .supports_server_capability(PROMPT_HISTORY_CAPABILITY)
    }

    /// Persist one submitted prompt to the project's history (fire-and-forget).
    /// Call it beside `editor.add_to_history` at every submit site.
    pub(crate) fn record_prompt_history(&self, text: &str) {
        if text.trim().is_empty() || !self.prompt_history_active() {
            return;
        }
        let client = self.client.clone();
        let active_session_id = self.active_session_id.clone();
        let display = text.to_string();
        tokio::spawn(async move {
            let _ = client
                .request_ok(DaemonCommand::AppendPromptHistory {
                    id: None,
                    active_session_id,
                    display,
                    rest: Map::default(),
                })
                .await;
        });
    }

    /// Seed the editor's up-arrow ring from the project's persisted history on
    /// attach. Best effort: a miss or timeout leaves recall empty rather than
    /// failing the attach.
    pub(crate) async fn seed_prompt_history(&self, view: &mut AgentView) {
        if !self.prompt_history_active() {
            return;
        }
        let Ok(data) = self
            .bounded_request(
                Duration::from_millis(UI_REQUEST_TIMEOUT_MS),
                DaemonCommand::GetPromptHistory {
                    id: None,
                    active_session_id: self.active_session_id.clone(),
                    limit: None,
                    rest: Map::default(),
                },
            )
            .await
        else {
            return;
        };
        let entries: Vec<String> = data
            .get("entries")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row.get("display").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if !entries.is_empty() {
            view.editor.seed_history(&entries);
        }
    }
}
