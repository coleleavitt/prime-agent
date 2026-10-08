//! The process's non-fatal auth notices (`pa_core::auth::AuthNotice`: a
//! refreshed login that could not be saved, a revoked login another one
//! replaces) for one session: each goes to attached clients once as a
//! `{"type": "auth_notice", "provider", "condition", "message"}` session
//! event. Additive: an older client ignores an unknown event type, and the
//! supervisor passes it through untouched, so neither the protocol version
//! nor the schema revision moves. Never a token value.

use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::worker::{EventPump, SessionCore};

/// The session event's type.
pub(crate) const AUTH_NOTICE_EVENT: &str = "auth_notice";

/// Broadcast one notice to the session's clients.
pub(crate) fn publish(
    core: &Arc<Mutex<SessionCore>>,
    events: &Arc<EventPump>,
    notice: &pa_core::auth::AuthNotice,
) {
    crate::user_bash::emit_session_event_frame(
        core,
        events,
        json!({
            "type": AUTH_NOTICE_EVENT,
            "provider": notice.provider,
            "condition": notice.condition,
            "message": notice.message,
        }),
    );
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn a_notice_rides_a_session_event() {
        let core = Arc::new(Mutex::new(SessionCore::test_core(None, "/tmp".to_string())));
        let events = Arc::new(EventPump::new());
        let mut frames = events.subscribe();

        publish(
            &core,
            &events,
            &pa_core::auth::AuthNotice {
                provider: "anthropic".to_string(),
                condition: "revoked:main".to_string(),
                message: "Your Anthropic login main was revoked; using pool. Run /login anthropic to restore it.".to_string(),
            },
        );

        let frame: Value = serde_json::from_slice(&frames.try_recv().unwrap().payload).unwrap();
        assert_eq!(frame["type"], json!("session_event"));
        assert_eq!(
            frame["event"],
            json!({
                "type": "auth_notice",
                "provider": "anthropic",
                "condition": "revoked:main",
                "message": "Your Anthropic login main was revoked; using pool. Run /login anthropic to restore it.",
            })
        );
    }
}
