//! The skill's adoption telemetry, handed to the embedding host.
//!
//! Two catalogued events (`pa-telemetry` schema v3): the first `get_state()`
//! of a host emits `computer_use_session_started`, and every App action
//! emits one `computer_use_action` with its outcome and duration. No event
//! carries app names, element text, or any other screen content.

use crate::error::ErrorCode;

/// One action's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Error(ErrorCode),
}

/// One telemetry event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryEvent {
    /// `computer_use_session_started {platform}`.
    SessionStarted { platform: &'static str },
    /// `computer_use_action {action, outcome, duration_ms[, error_code]}`.
    Action {
        action: &'static str,
        outcome: Outcome,
        duration_ms: u64,
    },
}

impl TelemetryEvent {
    /// The catalogued event name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            TelemetryEvent::SessionStarted { .. } => "computer_use_session_started",
            TelemetryEvent::Action { .. } => "computer_use_action",
        }
    }

    /// The event's properties as the catalogue spells them.
    #[must_use]
    pub fn properties(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut properties = serde_json::Map::new();
        match self {
            TelemetryEvent::SessionStarted { platform } => {
                properties.insert("platform".into(), (*platform).into());
            }
            TelemetryEvent::Action {
                action,
                outcome,
                duration_ms,
            } => {
                properties.insert("action".into(), (*action).into());
                let (outcome, error_code) = match outcome {
                    Outcome::Ok => ("ok", None),
                    Outcome::Error(code) => ("error", Some(code.as_str())),
                };
                properties.insert("outcome".into(), outcome.into());
                properties.insert("duration_ms".into(), (*duration_ms).into());
                if let Some(code) = error_code {
                    properties.insert("error_code".into(), code.into());
                }
            }
        }
        properties
    }
}

/// Where events go. Implementations must not block (they run on the
/// action's thread): queue the event and return.
pub trait TelemetrySink: Send + Sync {
    fn track(&self, event: TelemetryEvent);
}

/// The sink of a host without telemetry (an opted-out session).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoTelemetry;

impl TelemetrySink for NoTelemetry {
    fn track(&self, _event: TelemetryEvent) {}
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn events_carry_the_catalogued_properties() {
        let error = TelemetryEvent::Action {
            action: "click",
            outcome: Outcome::Error(ErrorCode::ElementStale),
            duration_ms: 12,
        };
        assert_eq!(error.name(), "computer_use_action");
        assert_eq!(
            serde_json::Value::Object(error.properties()),
            json!({"action": "click", "outcome": "error", "duration_ms": 12, "error_code": "ELEMENT_STALE"})
        );
        let started = TelemetryEvent::SessionStarted { platform: "mac" };
        assert_eq!(
            serde_json::Value::Object(started.properties()),
            json!({"platform": "mac"})
        );
    }
}
