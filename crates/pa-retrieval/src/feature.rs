//! The session feature: daemon-worker sessions render their harness digest
//! through the agent dir's shared reranker, and every agent stop runs the
//! retrieval tick. Every session also gets the brain (`docs/brain-design.md`):
//! the `brain.*` host requests and skill, and the digest's pins, withheld
//! superseded/excluded memories and synthesized summaries.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::HostRequestHandlers;
use pa_core::refinement::prompt_hook::HarnessPromptHook;
use pa_core::refinement::ranking::HarnessRerankHook;

use crate::brain::{self, BRAIN_SKILL, BrainPromptHook, BrainSession, CachedState, SynthScope};
use crate::handle::{RerankerHandle, shared_handle_for};

/// How often an agent stop may scan whether a synthesis is due.
const AUTO_SCAN_INTERVAL: Duration = Duration::from_mins(10);

/// Learning memory retrieval as a session feature (installed by the
/// composition root). Its reranker and tick serve daemon-worker sessions
/// only: daemonless (print-mode) sessions keep the native digest order and
/// never write the retrieval state.
#[derive(Default)]
pub struct RetrievalFeature {
    /// Serializes the ticks: each one advances the on-disk cursors.
    tick: Arc<Mutex<()>>,
    /// The brain state the digest hook reads (mtime-cached).
    brain_state: Arc<CachedState>,
    /// When an agent stop last scanned for a due synthesis.
    last_auto_scan: Mutex<Option<Instant>>,
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

impl RetrievalFeature {
    /// Start the automatic synthesis of the session's project and of all
    /// projects when due (top-level daemon sessions, at most one scan per
    /// [`AUTO_SCAN_INTERVAL`], off with `PRIME_AGENT_BRAIN_AUTO_SYNTHESIZE=0`).
    fn maybe_auto_synthesize(&self, context: &Arc<SessionFeatureContext>) {
        if context.daemonless || context.rlm_depth > 0 || brain::auto_disabled() {
            return;
        }
        {
            let mut last = self
                .last_auto_scan
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if last.is_some_and(|at| at.elapsed() < AUTO_SCAN_INTERVAL) {
                return;
            }
            *last = Some(Instant::now());
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(target: "pa_daemon::retrieval", "brain: no runtime for synthesis");
            return;
        };
        let session = BrainSession::of(context);
        runtime.spawn(async move {
            let due = tokio::task::spawn_blocking({
                let session = session.clone();
                move || {
                    [
                        SynthScope::Project(brain::project_of(&session.cwd)),
                        SynthScope::Global,
                    ]
                    .into_iter()
                    .filter(|scope| {
                        brain::due(
                            &session.agent_dir,
                            scope,
                            brain::summary_of(&session.agent_dir, scope).as_ref(),
                        )
                    })
                    .collect::<Vec<_>>()
                }
            })
            .await
            .unwrap_or_default();
            for scope in due {
                match brain::synthesize(session.clone(), scope, false).await {
                    Ok(summary) => tracing::info!(
                        target: "pa_daemon::retrieval",
                        lines = summary.lines.len(),
                        sources = summary.source_count,
                        "brain: synthesized"
                    ),
                    Err(error) => tracing::warn!(
                        target: "pa_daemon::retrieval",
                        error = %format!("{error:#}"),
                        "brain: synthesis failed"
                    ),
                }
            }
        });
    }
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

    fn harness_prompt_hook(
        &self,
        context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn HarnessPromptHook>> {
        Some(Arc::new(BrainPromptHook {
            agent_dir: context.agent_dir.clone(),
            session_id: context.session_id.clone(),
            project: brain::project_of(&context.cwd),
            state: Arc::clone(&self.brain_state),
        }))
    }

    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        brain::register(&BrainSession::of(context), handlers);
    }

    fn bundled_skills(&self) -> Vec<&'static str> {
        vec![BRAIN_SKILL]
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
                let _serialized = tick.lock().unwrap_or_else(PoisonError::into_inner);
                let demoted = brain::demote_superseded(&agent_dir, &handle);
                if demoted > 0 {
                    tracing::info!(
                        target: "pa_daemon::retrieval",
                        demoted,
                        "brain: superseded memories demoted"
                    );
                }
                crate::tick::run(&agent_dir, &handle);
            });
        if let Err(err) = spawned {
            tracing::warn!(target: "pa_daemon::retrieval", %err, "retrieval tick not started");
        }
        self.maybe_auto_synthesize(context);
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
