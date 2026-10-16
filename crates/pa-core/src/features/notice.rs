//! Feature notices: text a feature delivers to the model outside any turn
//! it is part of (a background check that finished and wants the agent to
//! look). The notice is a visible custom row; a busy session takes it as
//! steering, an idle one starts a turn on it. The session engine registers
//! its own delivery for every session (the agent's admit-or-steer), and an
//! embedding that owns the session's queue (the daemon worker) replaces it
//! with its own, so the woken turn runs through its lanes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use pa_types::session::CustomMessage;

/// The custom type of a feature notice row.
pub const FEATURE_NOTICE_CUSTOM_TYPE: &str = "feature_notice";

/// A notice one feature delivers to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureNotice {
    /// The delivering feature (`SessionFeature::name`).
    pub feature: String,
    /// What the model reads.
    pub text: String,
}

/// Where a session's feature notices go: the row to admit as a turn when the
/// session is idle, or to steer into the running one.
pub type FeatureNoticeSink = Arc<dyn Fn(CustomMessage) + Send + Sync>;

type NoticeSinks = Mutex<HashMap<String, Weak<dyn Fn(CustomMessage) + Send + Sync>>>;

fn notice_sinks() -> &'static NoticeSinks {
    static SINKS: OnceLock<NoticeSinks> = OnceLock::new();
    SINKS.get_or_init(NoticeSinks::default)
}

/// Route session `session_id`'s feature notices to `sink` while the caller
/// keeps it alive (the registry holds it weakly); replaces an earlier sink.
pub fn register_feature_notice_sink(session_id: &str, sink: &FeatureNoticeSink) {
    let mut sinks = notice_sinks()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    sinks.retain(|_, sink| sink.strong_count() > 0);
    sinks.insert(session_id.to_string(), Arc::downgrade(sink));
}

/// The row a notice is delivered as: visible, so the user sees why the
/// agent woke, with the delivering feature in its details.
#[must_use]
pub fn feature_notice_row(notice: &FeatureNotice) -> CustomMessage {
    CustomMessage {
        custom_type: FEATURE_NOTICE_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(notice.text.clone()),
        display: true,
        details: Some(serde_json::json!({ "feature": notice.feature })),
        timestamp: u64::try_from(pa_agent::now_ms()).unwrap_or_default(),
        rest: serde_json::Map::default(),
    }
}

/// Deliver `notice` to session `session_id`'s model: it wakes an idle
/// session (a turn starts on it) or steers into the running turn. `false`
/// when the session has no live sink (it ended, or was never built in this
/// process); the notice is then dropped.
pub fn deliver_feature_notice(session_id: &str, notice: &FeatureNotice) -> bool {
    let sink = notice_sinks()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(session_id)
        .and_then(Weak::upgrade);
    match sink {
        Some(sink) => {
            sink(feature_notice_row(notice));
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notice_reaches_the_live_sink_only() {
        let session = "notice-session";
        let notice = FeatureNotice {
            feature: "hooks".to_string(),
            text: "the check failed".to_string(),
        };
        assert!(!deliver_feature_notice(session, &notice));
        let seen: Arc<Mutex<Vec<CustomMessage>>> = Arc::default();
        let sink: FeatureNoticeSink = {
            let seen = Arc::clone(&seen);
            Arc::new(move |row| seen.lock().unwrap().push(row))
        };
        register_feature_notice_sink(session, &sink);
        assert!(deliver_feature_notice(session, &notice));
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].custom_type, FEATURE_NOTICE_CUSTOM_TYPE);
            assert!(seen[0].display);
            assert_eq!(
                seen[0].content,
                pa_types::ai::UserContent::Text("the check failed".to_string())
            );
            assert_eq!(
                seen[0].details,
                Some(serde_json::json!({ "feature": "hooks" }))
            );
        }
        drop(sink);
        assert!(!deliver_feature_notice(session, &notice));
    }
}
