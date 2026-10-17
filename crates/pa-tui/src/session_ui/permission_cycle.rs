//! The plan-mode key as Claude Code's permission-mode cycle. A feature that
//! owns permission modes publishes, in its `permissions` feature status,
//! the session commands the next press sends (`cycle`, say
//! `["/permissions mode plan", "/plan on"]`); the key sends them in order
//! instead of the plain `/plan` toggle. Without such a status (the native
//! product, or a configuration where the cycle would only toggle plan mode)
//! the key stays the plan toggle.

use serde_json::Value;

/// The feature status that carries the cycle.
pub(super) const PERMISSIONS_FEATURE: &str = "permissions";

/// The most commands one press sends.
const MAX_CYCLE_COMMANDS: usize = 4;

/// The session commands a `permissions` status asks the key to send: slash
/// commands only, at most [`MAX_CYCLE_COMMANDS`].
pub(super) fn cycle_of(status: &Value) -> Vec<String> {
    status
        .get("cycle")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|command| command.starts_with('/') && !command.contains('\n'))
        .take(MAX_CYCLE_COMMANDS)
        .map(str::to_string)
        .collect()
}

/// The cycle a roster summary carries
/// (`featureStatus.permissions.status.cycle`).
pub(super) fn cycle_from_summary(summary: &Value) -> Vec<String> {
    summary
        .get("featureStatus")
        .and_then(|statuses| statuses.get(PERMISSIONS_FEATURE))
        .and_then(|entry| entry.get("status"))
        .map(cycle_of)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_cycle_keeps_slash_commands_only() {
        let status = json!({
            "mode": "acceptEdits",
            "cycle": ["/permissions mode plan", "/plan on", "rm -rf /", 3, "/x\n/y"],
        });
        assert_eq!(
            cycle_of(&status),
            vec!["/permissions mode plan".to_string(), "/plan on".to_string()]
        );
        assert_eq!(
            cycle_from_summary(
                &json!({ "featureStatus": { "permissions": { "status": status } } })
            ),
            cycle_of(&status)
        );
        assert!(cycle_from_summary(&json!({})).is_empty());
        assert!(cycle_of(&json!({ "mode": "default" })).is_empty());
    }
}
