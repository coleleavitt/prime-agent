//! The session feature: daemon-worker sessions render their harness digest
//! through the agent dir's shared reranker, and every agent stop runs the
//! retrieval tick.

use std::sync::{Arc, Mutex};

use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::refinement::ranking::HarnessRerankHook;

use crate::handle::{RerankerHandle, shared_handle_for};

/// Learning memory retrieval as a session feature (installed by the
/// composition root). It serves daemon-worker sessions only: daemonless
/// (print-mode) sessions keep the native digest order and never write the
/// retrieval state.
#[derive(Default)]
pub struct RetrievalFeature {
    /// Serializes the ticks: each one advances the on-disk cursors.
    tick: Arc<Mutex<()>>,
}

impl RetrievalFeature {
    /// The feature, with no tick running.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// The shared reranker of `context`'s agent dir, for a daemon-worker
/// session; `None` for a daemonless one, or when flyrag fails to
/// initialise (the digest keeps the native order).
fn session_reranker(context: &SessionFeatureContext) -> Option<RerankerHandle> {
    if context.daemonless {
        return None;
    }
    shared_handle_for(&context.agent_dir)
        .inspect_err(|err| {
            tracing::warn!(
                target: "pa_daemon::retrieval",
                %err,
                "flyrag reranker failed to initialise; falling back to native sort"
            );
        })
        .ok()
}

impl SessionFeature for RetrievalFeature {
    fn name(&self) -> &'static str {
        "retrieval"
    }

    fn harness_rerank_hook(
        &self,
        context: &Arc<SessionFeatureContext>,
    ) -> Option<HarnessRerankHook> {
        session_reranker(context).map(|handle| handle.as_rerank_hook())
    }

    fn on_agent_stop(&self, context: &Arc<SessionFeatureContext>) {
        let Some(handle) = session_reranker(context) else {
            return;
        };
        let agent_dir = context.agent_dir.clone();
        let tick = Arc::clone(&self.tick);
        let spawned = std::thread::Builder::new()
            .name("retrieval-tick".to_string())
            .spawn(move || {
                let _serialized = tick
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                crate::tick::run(&agent_dir, &handle);
            });
        if let Err(err) = spawned {
            tracing::warn!(target: "pa_daemon::retrieval", %err, "retrieval tick not started");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(agent_dir: &std::path::Path, daemonless: bool) -> Arc<SessionFeatureContext> {
        Arc::new(SessionFeatureContext {
            agent_dir: agent_dir.to_path_buf(),
            cwd: agent_dir.to_path_buf(),
            session_id: "s1".to_string(),
            python_skill_import_names: Vec::new(),
            model: serde_json::from_value(serde_json::json!({
                "id": "m1", "name": "M1", "api": "test", "provider": "p1",
                "baseUrl": "http://localhost", "reasoning": false,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                "contextWindow": 1000, "maxTokens": 100
            }))
            .unwrap(),
            telemetry: None,
            rlm_depth: 0,
            session_artifact_dir: None,
            daemonless,
        })
    }

    /// Only daemon-worker sessions rerank their digest; a daemonless one
    /// keeps the native order.
    #[test]
    fn only_daemon_sessions_get_the_rerank_hook() {
        let feature = RetrievalFeature::new();
        let print = tempfile::tempdir().unwrap();
        assert!(
            feature
                .harness_rerank_hook(&context(print.path(), true))
                .is_none()
        );
        let worker = tempfile::tempdir().unwrap();
        assert!(
            feature
                .harness_rerank_hook(&context(worker.path(), false))
                .is_some()
        );
    }
}
