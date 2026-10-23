//! Installed features' live session status (the `pa_core::features`
//! status seam): each publication replaces the feature's entry in the
//! session's roster summary (`featureStatus.<feature> = {line, status}`,
//! dropped on passivation) and goes to attached clients as a
//! `{"type": "feature_status", "feature", "line", "status"}` session event,
//! which also schedules the roster push. Both are additive: older clients
//! ignore an unknown event type and an unknown summary key.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::worker::{EventPump, SessionCore};

/// The session event's type.
pub(crate) const FEATURE_STATUS_EVENT: &str = "feature_status";

/// Record and broadcast one feature status.
pub(crate) fn publish(
    core: &Arc<Mutex<SessionCore>>,
    events: &Arc<EventPump>,
    status: pa_core::features::FeatureStatus,
) {
    {
        let mut core = core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        core.feature_status.insert(
            status.feature.clone(),
            json!({ "line": status.line, "status": status.status }),
        );
    }
    crate::user_bash::emit_session_event_frame(
        core,
        events,
        json!({
            "type": FEATURE_STATUS_EVENT,
            "feature": status.feature,
            "line": status.line.map_or(Value::Null, Value::from),
            "status": status.status,
        }),
    );
}

/// `answer_feature_confirmation`: answer a question the session's hooks or
/// plugins put to the user (`pa_core::features::confirm`). Answers
/// `{"answered": false}` when it is no longer pending (already answered by
/// another client, timed out, or the action was aborted).
pub(crate) fn answer_feature_confirmation(payload: &Value) -> crate::protocol::DaemonResponse {
    const COMMAND: &str = "answer_feature_confirmation";
    let field = |key: &str| payload.get(key).and_then(Value::as_str).unwrap_or_default();
    let (session_id, confirmation_id) = (field("activeSessionId"), field("confirmationId"));
    let Some(allow) = payload.get("allow").and_then(Value::as_bool) else {
        return crate::protocol::response_failure(None, COMMAND, "allow is required", None);
    };
    if session_id.is_empty() || confirmation_id.is_empty() {
        return crate::protocol::response_failure(
            None,
            COMMAND,
            "activeSessionId and confirmationId are required",
            None,
        );
    }
    // `remember` (optional, additive): take the question's offer to remember
    // the answer (a permission rule); a question without one ignores it.
    let remember = payload
        .get("remember")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let answered = pa_core::features::answer_confirmation_remembering(
        session_id,
        confirmation_id,
        allow,
        remember,
    );
    crate::protocol::response_success(None, COMMAND, Some(json!({ "answered": answered })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_lands_on_the_summary_and_rides_a_roster_triggering_event() {
        let core = Arc::new(Mutex::new(SessionCore::test_core(None, "/tmp".to_string())));
        let events = Arc::new(EventPump::new());
        let mut frames = events.subscribe();
        let publish_one = |line: Option<&str>, phase: &str| {
            publish(
                &core,
                &events,
                pa_core::features::FeatureStatus {
                    feature: "stub".to_string(),
                    line: line.map(str::to_string),
                    status: json!({ "phase": phase }),
                },
            );
        };
        let summary = |core: &Arc<Mutex<SessionCore>>| {
            let core = core.lock().unwrap();
            serde_json::to_value(crate::worker::session_summary(
                &core,
                crate::worker::SummaryInputs {
                    thinking_level: "off".to_string(),
                    model: None,
                    model_fallback_message: None,
                    bash_running: false,
                    quota_parked: false,
                    subagents_running: false,
                    model_context_window: None,
                    progress_note: None,
                },
            ))
            .unwrap()
        };
        assert!(
            summary(&core).get("featureStatus").is_none(),
            "absent until published"
        );
        publish_one(Some("stub: running"), "running");
        publish_one(None, "done");
        assert_eq!(
            summary(&core)["featureStatus"],
            json!({ "stub": { "line": null, "status": { "phase": "done" } } })
        );
        let first = frames.try_recv().unwrap();
        let frame: Value = serde_json::from_slice(&first.payload).unwrap();
        assert_eq!(frame["type"], json!("session_event"));
        assert_eq!(
            frame["event"],
            json!({ "type": "feature_status", "feature": "stub", "line": "stub: running", "status": { "phase": "running" } })
        );
        assert!(crate::roster_activity::frame_triggers_roster_flush(&first));
    }

    #[tokio::test]
    async fn a_confirmation_answer_is_validated_and_reaches_the_pending_question() {
        let missing = answer_feature_confirmation(&json!({ "activeSessionId": "s" }));
        assert!(!missing.success);
        assert_eq!(missing.error.as_deref(), Some("allow is required"));
        let unnamed = answer_feature_confirmation(&json!({ "allow": true }));
        assert_eq!(
            unnamed.error.as_deref(),
            Some("activeSessionId and confirmationId are required")
        );
        let stale = answer_feature_confirmation(&json!({
            "activeSessionId": "daemon-confirm", "confirmationId": "confirm-0", "allow": true
        }));
        assert!(stale.success);
        assert_eq!(stale.data, Some(json!({ "answered": false })));

        let _surface = pa_core::features::register_confirmation_surface("daemon-confirm");
        let asking = tokio::spawn(pa_core::features::confirm(
            "daemon-confirm",
            "Run `x`",
            "why",
            std::time::Duration::from_secs(30),
        ));
        let id = loop {
            if let Some(question) = pa_core::features::pending_confirmations("daemon-confirm").pop()
            {
                break question.id;
            }
            tokio::task::yield_now().await;
        };
        let answered = answer_feature_confirmation(&json!({
            "activeSessionId": "daemon-confirm", "confirmationId": id, "allow": false
        }));
        assert_eq!(answered.data, Some(json!({ "answered": true })));
        assert_eq!(
            asking.await.unwrap(),
            pa_core::features::ConfirmationAnswer::Denied
        );

        // `remember` takes the question's offer to remember the answer.
        let offering = tokio::spawn(pa_core::features::confirm_offering(
            "daemon-confirm",
            "Run `npm test`",
            "why",
            Some("Bash(npm test *)"),
            std::time::Duration::from_secs(30),
        ));
        let id = loop {
            if let Some(question) = pa_core::features::pending_confirmations("daemon-confirm").pop()
            {
                break question.id;
            }
            tokio::task::yield_now().await;
        };
        let remembered = answer_feature_confirmation(&json!({
            "activeSessionId": "daemon-confirm", "confirmationId": id, "allow": true, "remember": true
        }));
        assert_eq!(remembered.data, Some(json!({ "answered": true })));
        assert_eq!(
            offering.await.unwrap(),
            pa_core::features::ConfirmationAnswer::AllowedAlways
        );
    }
}
