//! The worker side of Claude Code's side-model session UX (capability
//! `side_ux`):
//!
//! - The generated session title: a session's first real prompt (not a
//!   slash command, not an injected row or agent message, at least
//!   [`TITLE_MIN_CHARS`] characters) asks the engine for a title once; it is
//!   recorded as an `ai_title` custom row only while the session has neither
//!   a user name nor an earlier title, so a user's `/name` always wins. The
//!   catalog folds the row into `aiTitle`; titles show `name`, else
//!   `aiTitle`, else the first message.
//! - The prompt suggestion: after a run's true end, the engine predicts the
//!   next prompt; a suggestion reaches the attached clients as a
//!   `prompt_suggestion` event while the session is still idle.
//! - `session_recap` (`/recap`, the away summary) and
//!   `generate_session_name` (`/rename` without a name) answer one
//!   side-model call each.
//! - `blockedOn`: what the session waits on the user for (Claude Code's
//!   "Waiting on permission: X"), derived from the pending confirmations.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use pa_core::session_engine::side_ux::TITLE_MIN_CHARS;
use pa_types::sync::MutexExt;
use serde_json::{Map, Value, json};

use crate::engine::{SessionEngine, SideUxOutcome, SideUxRequest};
use crate::protocol::{DaemonResponse, response_failure, response_success};
use crate::worker::Worker;

pub(crate) const SESSION_RECAP: &str = "session_recap";
pub(crate) const GENERATE_SESSION_NAME: &str = "generate_session_name";
/// The worker event carrying a predicted next prompt: `{ type, text }`.
pub(crate) const PROMPT_SUGGESTION_EVENT: &str = "prompt_suggestion";

/// The confirmation feature status (`pa_core::features::CONFIRMATION_FEATURE`).
const CONFIRMATION_FEATURE: &str = pa_core::features::CONFIRMATION_FEATURE;
/// The longest action a `blockedOn` line quotes.
const BLOCKED_ACTION_MAX_CHARS: usize = 80;

/// What the session waits on the user for: the oldest pending confirmation
/// ("Waiting on permission: <action's first line>", Claude Code's status
/// detail), else the first feature status (by name) whose `status` declares
/// a `blockedOn` string; `None` when nothing waits.
pub(crate) fn blocked_on(feature_status: &Map<String, Value>) -> Option<String> {
    let action = feature_status
        .get(CONFIRMATION_FEATURE)
        .and_then(|entry| entry.get("status"))
        .and_then(|status| status.get("confirmations"))
        .and_then(Value::as_array)
        .and_then(|pending| pending.first())
        .and_then(|first| first.get("action"))
        .and_then(Value::as_str);
    if let Some(action) = action {
        let line = action.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
        let mut clipped: String = line.chars().take(BLOCKED_ACTION_MAX_CHARS).collect();
        if line.chars().count() > BLOCKED_ACTION_MAX_CHARS {
            clipped.push('\u{2026}');
        }
        return Some(if clipped.is_empty() {
            "Waiting on permission".to_string()
        } else {
            format!("Waiting on permission: {clipped}")
        });
    }
    let mut features: Vec<&String> = feature_status.keys().collect();
    features.sort();
    features.into_iter().find_map(|feature| {
        feature_status
            .get(feature)?
            .get("status")?
            .get("blockedOn")?
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
}

/// The prompt text a title may name: an ordinary user prompt (no injected
/// custom row, no agent message), not a slash command, long enough.
pub(crate) fn title_candidate(
    message: &str,
    custom_message: Option<&Value>,
    agent_message: Option<&str>,
) -> Option<String> {
    if custom_message.is_some() || agent_message.is_some() {
        return None;
    }
    let trimmed = message.trim();
    if trimmed.starts_with('/') || trimmed.chars().count() < TITLE_MIN_CHARS {
        return None;
    }
    Some(trimmed.to_string())
}

/// Sessions whose title was asked for in this process: one attempt per
/// session (a failed call is forgotten so the next prompt retries, like
/// Claude Code's).
fn title_attempts() -> &'static Mutex<HashSet<String>> {
    static ATTEMPTS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ATTEMPTS.get_or_init(Mutex::default)
}

impl Worker {
    /// Ask for the session title when `message` is the session's first
    /// titleable prompt and the session has no name or title yet.
    pub(crate) fn maybe_generate_title(
        &self,
        engine: &Arc<dyn SessionEngine>,
        message: &str,
        custom_message: Option<&Value>,
        agent_message: Option<&str>,
    ) {
        let Some(text) = title_candidate(message, custom_message, agent_message) else {
            return;
        };
        let session_id = {
            let core = self.core.lock_or_recover();
            let Some(store) = core.store.as_ref() else {
                return;
            };
            if store.path.as_os_str().is_empty()
                || store.session_name().is_some()
                || store.ai_title().is_some()
            {
                return;
            }
            store.session_id().to_string()
        };
        if !title_attempts().lock_or_recover().insert(session_id.clone()) {
            return;
        }
        let engine = Arc::clone(engine);
        let core = Arc::clone(&self.core);
        let roster = self.roster_pushes.clone();
        tokio::spawn(async move {
            let request = SideUxRequest::SessionTitle {
                first_message: text,
            };
            let outcome = tokio::task::spawn_blocking(move || engine.run_side_ux(request))
                .await
                .unwrap_or_else(|error| SideUxOutcome::Failed(error.to_string()));
            let title = match outcome {
                SideUxOutcome::Text(title) => title,
                SideUxOutcome::Failed(error) => {
                    eprintln!("pa-daemon: session title generation failed: {error}");
                    title_attempts().lock_or_recover().remove(&session_id);
                    return;
                }
                SideUxOutcome::Nothing | SideUxOutcome::Disabled | SideUxOutcome::Unsupported => {
                    return;
                }
            };
            let recorded = {
                let mut core = core.lock_or_recover();
                match core.store.as_mut() {
                    // The user's own name (or a title that landed meanwhile)
                    // wins; a replaced session never takes this one's title.
                    Some(store)
                        if store.session_id() == session_id
                            && store.session_name().is_none()
                            && store.ai_title().is_none() =>
                    {
                        store
                            .persist_entry(
                                "custom",
                                json!({
                                    "customType": crate::session_store::AI_TITLE_CUSTOM_TYPE,
                                    "data": { "title": title },
                                }),
                            )
                            .is_ok()
                    }
                    _ => false,
                }
            };
            if recorded {
                roster.push();
            }
        });
    }

    /// Predict the next prompt after a run's true end; the suggestion
    /// reaches the clients only while the session is still idle.
    pub(crate) fn maybe_suggest_prompt(&self, engine: &Arc<dyn SessionEngine>) {
        let session_id = {
            let core = self.core.lock_or_recover();
            if core.attached_client_ids.is_empty()
                || blocked_on(&core.feature_status).is_some()
                || core.has_ongoing_work()
            {
                return;
            }
            core.active_session_id.clone()
        };
        let engine = Arc::clone(engine);
        let core = Arc::clone(&self.core);
        let events = Arc::clone(&self.events);
        tokio::spawn(async move {
            let outcome =
                tokio::task::spawn_blocking(move || engine.run_side_ux(SideUxRequest::PromptSuggestion))
                    .await
                    .unwrap_or(SideUxOutcome::Nothing);
            let SideUxOutcome::Text(text) = outcome else {
                return;
            };
            {
                let core = core.lock_or_recover();
                if core.active_session_id != session_id || core.has_ongoing_work() {
                    return;
                }
            }
            crate::worker::emit_worker_event_with(
                &core,
                &events,
                json!({ "type": PROMPT_SUGGESTION_EVENT, "text": text }),
            );
        });
    }

    /// `session_recap`: the away summary now (`/recap`).
    pub(crate) async fn handle_session_recap(&self) -> DaemonResponse {
        if let Err(response) = self.require_created(SESSION_RECAP) {
            return response;
        }
        match self.side_ux_call(SideUxRequest::Recap).await {
            SideUxOutcome::Text(text) => {
                response_success(None, SESSION_RECAP, Some(json!({ "text": text })))
            }
            SideUxOutcome::Nothing => response_failure(
                None,
                SESSION_RECAP,
                "Nothing to recap yet \u{2014} send a message first.",
                None,
            ),
            other => response_failure(None, SESSION_RECAP, &unavailable("a recap", other), None),
        }
    }

    /// `generate_session_name`: a kebab-case name for `/rename` without one.
    pub(crate) async fn handle_generate_session_name(&self) -> DaemonResponse {
        if let Err(response) = self.require_created(GENERATE_SESSION_NAME) {
            return response;
        }
        match self.side_ux_call(SideUxRequest::SessionName).await {
            SideUxOutcome::Text(name) => {
                response_success(None, GENERATE_SESSION_NAME, Some(json!({ "name": name })))
            }
            SideUxOutcome::Nothing => response_failure(
                None,
                GENERATE_SESSION_NAME,
                "Could not generate a name: no conversation context yet. Usage: /rename <name>",
                None,
            ),
            other => response_failure(
                None,
                GENERATE_SESSION_NAME,
                &unavailable("a name", other),
                None,
            ),
        }
    }

    async fn side_ux_call(&self, request: SideUxRequest) -> SideUxOutcome {
        let engine = Arc::clone(&self.engine);
        tokio::task::spawn_blocking(move || engine.run_side_ux(request))
            .await
            .unwrap_or_else(|error| SideUxOutcome::Failed(error.to_string()))
    }
}

/// The failure line for an outcome that produced no text.
fn unavailable(what: &str, outcome: SideUxOutcome) -> String {
    match outcome {
        SideUxOutcome::Failed(error) => format!("Couldn't generate {what}: {error}"),
        SideUxOutcome::Disabled => format!("Couldn't generate {what}: the setting is off"),
        SideUxOutcome::Unsupported | SideUxOutcome::Nothing | SideUxOutcome::Text(_) => {
            format!("Couldn't generate {what}: this session has no model to ask")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_confirmation_blocks_with_its_first_action_line() {
        let status = json!({
            "confirmation": { "line": "waiting", "status": { "confirmations": [
                { "id": "1", "action": "bash: rm -rf build\nsecond line", "reason": "r" },
                { "id": "2", "action": "later", "reason": "r" },
            ] } },
        });
        assert_eq!(
            blocked_on(status.as_object().unwrap()).as_deref(),
            Some("Waiting on permission: bash: rm -rf build")
        );
        let long = json!({ "confirmation": { "status": { "confirmations": [
            { "id": "1", "action": "x".repeat(90) },
        ] } } });
        assert_eq!(
            blocked_on(long.as_object().unwrap()),
            Some(format!("Waiting on permission: {}\u{2026}", "x".repeat(80)))
        );
    }

    #[test]
    fn a_cleared_confirmation_or_none_leaves_the_session_unblocked() {
        assert_eq!(blocked_on(&Map::new()), None);
        let cleared = json!({ "confirmation": { "line": null, "status": { "confirmations": [] } } });
        assert_eq!(blocked_on(cleared.as_object().unwrap()), None);
    }

    #[test]
    fn a_feature_status_may_declare_what_blocks_the_session() {
        let status = json!({
            "zeta": { "status": { "blockedOn": "Waiting on z" } },
            "alpha": { "status": { "blockedOn": "  Waiting for your answer: which branch?  " } },
            "beta": { "status": { "blockedOn": "" } },
        });
        assert_eq!(
            blocked_on(status.as_object().unwrap()).as_deref(),
            Some("Waiting for your answer: which branch?")
        );
    }

    #[test]
    fn only_ordinary_long_enough_prompts_are_titled() {
        assert_eq!(
            title_candidate("  why do roster deltas reorder? ", None, None).as_deref(),
            Some("why do roster deltas reorder?")
        );
        assert_eq!(title_candidate("hi there", None, None), None);
        assert_eq!(title_candidate("/compact keep the plan", None, None), None);
        assert_eq!(
            title_candidate("a long injected notice", Some(&json!({})), None),
            None
        );
        assert_eq!(
            title_candidate("a long agent message", None, Some("m1")),
            None
        );
    }
}
