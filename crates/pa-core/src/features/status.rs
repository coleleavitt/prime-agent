//! Feature statuses: what a feature publishes for one session (a plugin's
//! panel rows, a pending confirmation) and the embedding's sink that carries
//! them to the session-event surface. A status published before the
//! session's sink registers is held for it.

use std::sync::{Arc, OnceLock};

/// A feature's live status for one session: what a session-event surface
/// (the daemon's `feature_status` event and roster summary, the agents view)
/// shows. Replaces the feature's previous status for that session.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureStatus {
    /// The publishing feature (`SessionFeature::name`).
    pub feature: String,
    /// A one-line human summary the agents view shows; `None` clears it.
    pub line: Option<String>,
    /// The feature's own structured status (opaque to native code).
    pub status: serde_json::Value,
}

/// Where a session's feature statuses go (the embedding's event surface).
pub type FeatureStatusSink = Arc<dyn Fn(FeatureStatus) + Send + Sync>;

type StatusSinks = std::sync::Mutex<
    std::collections::HashMap<String, std::sync::Weak<dyn Fn(FeatureStatus) + Send + Sync>>,
>;

fn status_sinks() -> &'static StatusSinks {
    static SINKS: OnceLock<StatusSinks> = OnceLock::new();
    SINKS.get_or_init(StatusSinks::default)
}

/// Statuses published for a session before its sink registered, latest
/// per feature: a feature may publish while the session is still being
/// built (its load runs off the creation path), before the embedding
/// registers the session's sink.
type PendingStatuses = std::sync::Mutex<std::collections::HashMap<String, Vec<FeatureStatus>>>;

/// How many sessions may hold undelivered statuses at once; a session
/// beyond it (an embedding that never registers sinks) keeps none.
const MAX_PENDING_STATUS_SESSIONS: usize = 64;

fn pending_statuses() -> &'static PendingStatuses {
    static PENDING: OnceLock<PendingStatuses> = OnceLock::new();
    PENDING.get_or_init(PendingStatuses::default)
}

/// Route session `session_id`'s feature statuses to `sink` while the caller
/// keeps it alive (the registry holds it weakly); replaces an earlier sink.
/// Statuses published for the session before any sink registered are
/// delivered to it now, latest per feature.
pub fn register_feature_status_sink(session_id: &str, sink: &FeatureStatusSink) {
    {
        let mut sinks = status_sinks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sinks.retain(|_, sink| sink.strong_count() > 0);
        sinks.insert(session_id.to_string(), Arc::downgrade(sink));
    }
    let held = pending_statuses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(session_id)
        .unwrap_or_default();
    for status in held {
        sink(status);
    }
}

/// Publish a feature's status for a session; `false` when the session has
/// no live sink yet (the latest status per feature is held for the sink
/// that registers next, see [`register_feature_status_sink`]). Never
/// blocks on the surface beyond the sink's own call.
pub fn publish_feature_status(session_id: &str, status: FeatureStatus) -> bool {
    let sink = status_sinks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(session_id)
        .and_then(std::sync::Weak::upgrade);
    if let Some(sink) = sink {
        sink(status);
        return true;
    }
    let mut pending = pending_statuses()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if pending.len() >= MAX_PENDING_STATUS_SESSIONS && !pending.contains_key(session_id) {
        return false;
    }
    let held = pending.entry(session_id.to_string()).or_default();
    held.retain(|earlier| earlier.feature != status.feature);
    held.push(status);
    false
}
