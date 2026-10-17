//! The session modes a session publishes as feature statuses: `brief`
//! (`{"enabled": bool}`, the view collapses the assistant text of turns that
//! sent the user a message) and `coordinator` (`{"enabled": true}`, a fork
//! that keeps the original running is refused, as Claude Code refuses
//! `/fork` in coordinator sessions).

use serde_json::Value;

/// Wire twin of `pa_core::session_engine::brief_mode::BRIEF_FEATURE`.
pub(super) const BRIEF_FEATURE: &str = "brief";
/// Wire twin of `pa_core::session_engine::coordinator::COORDINATOR_FEATURE`.
pub(super) const COORDINATOR_FEATURE: &str = "coordinator";

/// Claude Code's `COORDINATOR_FORK_REFUSAL`, pointing at the in-place form.
pub(super) const COORDINATOR_FORK_REFUSAL: &str =
    "Forking is not available in coordinator sessions. Use /fork --replace instead.";

/// A mode status's `enabled` flag.
pub(super) fn enabled(status: &Value) -> bool {
    status.get("enabled").and_then(Value::as_bool) == Some(true)
}

/// The mode a roster summary carries (`featureStatus.<feature>.status`; a
/// session that never published it is off); `None` when the summary has no
/// statuses at all (a passivated summary drops them), so the mode stays.
pub(super) fn enabled_in_summary(summary: &Value, feature: &str) -> Option<bool> {
    let statuses = summary.get("featureStatus")?.as_object()?;
    Some(
        statuses
            .get(feature)
            .and_then(|entry| entry.get("status"))
            .is_some_and(enabled),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn statuses_read_their_enabled_flag() {
        assert!(enabled(&json!({ "enabled": true })));
        assert!(!enabled(&json!({ "enabled": false })));
        assert!(!enabled(&json!({})));
        let summary = json!({ "featureStatus": { "brief": { "line": "brief mode", "status": { "enabled": true } } } });
        assert_eq!(enabled_in_summary(&summary, BRIEF_FEATURE), Some(true));
        assert_eq!(
            enabled_in_summary(&summary, COORDINATOR_FEATURE),
            Some(false)
        );
        assert_eq!(enabled_in_summary(&json!({}), BRIEF_FEATURE), None);
    }
}
