//! Installed features' live session status (the `pa_core::features`
//! status seam): each publication replaces the feature's entry in the
//! session's roster summary (`featureStatus.<feature> = {line, status}`,
//! dropped on passivation) and goes to attached clients as a
//! `{"type": "feature_status", "feature", "line", "status"}` session event,
//! which also schedules the roster push. Both are additive: older clients
//! ignore an unknown event type and an unknown summary key.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

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
                &core, "off", None, None, false, false, false,
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
}
