//! The session feature: the request-context hook, the `ctx.*` requests and
//! skill, the reduce nudge after tool batches, and the reset of folded
//! history after a native compaction. Top-level sessions only.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use pa_agent::types::AgentMessage;
use pa_core::features::{
    CompactionObservation,
    FeatureFuture,
    RequestContextHook,
    SessionFeature,
    SessionFeatureContext,
    ToolBatchObservation,
};
use pa_core::kernel::shared::HostRequestHandlers;

use crate::session::{ContextSession, switched_off};
use crate::{host, nudge};

/// `0`/`off`/`false` turns the context manager off.
pub const CONTEXT_MANAGER_ENV: &str = "PRIME_AGENT_CONTEXT_MANAGER";
/// The bundled skill (`skills/.features/ctx`).
pub const CTX_SKILL: &str = "ctx";

/// The context manager as a session feature.
#[derive(Default)]
pub struct ContextFeature {
    sessions: Mutex<HashMap<String, Arc<ContextSession>>>,
}

impl ContextFeature {
    /// The feature with no session yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn enabled(context: &SessionFeatureContext) -> bool {
        context.rlm_depth == 0
            && context.model.context_window > 0
            && !switched_off(CONTEXT_MANAGER_ENV)
    }

    fn session(&self, context: &SessionFeatureContext) -> Arc<ContextSession> {
        Arc::clone(
            self.sessions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(context.session_id.clone())
                .or_insert_with(|| Arc::new(ContextSession::new(context))),
        )
    }

    fn existing(&self, session_id: &str) -> Option<Arc<ContextSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id)
            .cloned()
    }
}

struct Hook(Arc<ContextSession>);

impl RequestContextHook for Hook {
    fn transform(
        &self,
        messages: Vec<AgentMessage>,
    ) -> FeatureFuture<anyhow::Result<Vec<AgentMessage>>> {
        let session = Arc::clone(&self.0);
        Box::pin(async move { Ok(session.transform(messages)) })
    }
}

impl SessionFeature for ContextFeature {
    fn name(&self) -> &'static str {
        "context"
    }

    fn request_context_hook(
        &self,
        context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn RequestContextHook>> {
        Self::enabled(context)
            .then(|| Arc::new(Hook(self.session(context))) as Arc<dyn RequestContextHook>)
    }

    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        if Self::enabled(context) {
            host::register(&self.session(context), handlers);
        }
    }

    fn bundled_skills(&self) -> Vec<&'static str> {
        vec![CTX_SKILL]
    }

    fn session_skill_visible(&self, context: &SessionFeatureContext, skill_name: &str) -> bool {
        skill_name != CTX_SKILL || Self::enabled(context)
    }

    fn after_tool_batch(
        &self,
        context: &Arc<SessionFeatureContext>,
        _batch: &ToolBatchObservation,
    ) -> FeatureFuture<Option<String>> {
        let text = self.existing(&context.session_id).and_then(|session| {
            let report = session.snapshot()?.report;
            session.update(|state| nudge::nudge(&report, &mut state.nudge))
        });
        Box::pin(async move { text })
    }

    fn after_compaction(
        &self,
        context: &Arc<SessionFeatureContext>,
        _compaction: &CompactionObservation,
        _summary: &str,
    ) {
        if let Some(session) = self.existing(&context.session_id) {
            session.update(crate::state::ContextState::clear_compartments);
        }
    }

    fn on_session_end(&self, context: &Arc<SessionFeatureContext>, _reason: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&context.session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::tests::{tool_result, user};

    fn context(
        rlm_depth: u32,
        artifacts: Option<std::path::PathBuf>,
    ) -> Arc<SessionFeatureContext> {
        Arc::new(SessionFeatureContext {
            agent_dir: std::path::PathBuf::from("/nonexistent-agent"),
            cwd: std::path::PathBuf::from("/nonexistent-cwd"),
            session_id: "s1".to_string(),
            python_skill_import_names: Vec::new(),
            model: serde_json::from_value(serde_json::json!({
                "id": "m1", "name": "M1", "api": "test", "provider": "p1",
                "baseUrl": "http://localhost", "reasoning": false,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                "contextWindow": 200_000, "maxTokens": 100
            }))
            .unwrap(),
            telemetry: None,
            rlm_depth,
            session_artifact_dir: artifacts,
            daemonless: true,
        })
    }

    #[test]
    fn children_are_left_alone() {
        let feature = ContextFeature::new();
        assert!(feature.request_context_hook(&context(1, None)).is_none());
        assert!(!feature.session_skill_visible(&context(1, None), CTX_SKILL));
        assert!(feature.session_skill_visible(&context(1, None), "other"));
    }

    /// The hook tags the request, a `ctx.reduce` queued through the
    /// registered handler lands after `ctx.flush`, and the state survives a
    /// new process (resume) byte for byte.
    #[tokio::test]
    async fn the_hook_handlers_and_resume_agree() {
        let artifacts = tempfile::tempdir().unwrap();
        let ctx = context(0, Some(artifacts.path().to_path_buf()));
        let feature = ContextFeature::new();
        let hook = feature.request_context_hook(&ctx).unwrap();
        let mut handlers = HostRequestHandlers::new();
        feature.register_host_handlers(&ctx, &mut handlers);
        let messages: Vec<AgentMessage> = (0..6)
            .flat_map(|turn: i64| {
                [
                    user(&format!("task {turn}"), turn * 10),
                    tool_result(&format!("c{turn}"), &format!("out {turn}"), turn * 10 + 1),
                ]
            })
            .collect();
        let first = hook.transform(messages.clone()).await.unwrap();
        assert_eq!(crate::message::text(&first[1]), "§2§ out 0");

        let call = |request: &'static str, data: serde_json::Value| {
            let handler = handlers.get(request).unwrap().clone();
            async move {
                handler(pa_core::kernel::shared::HostRequestPayload { data })
                    .await
                    .unwrap()
            }
        };
        let reply = call(host::REDUCE_REQUEST, serde_json::json!({"tags": "2"})).await;
        assert_eq!(reply["queued"], serde_json::json!([2]));
        call(host::FLUSH_REQUEST, serde_json::json!({})).await;
        let second = hook.transform(messages.clone()).await.unwrap();
        assert_eq!(crate::message::text(&second[1]), "[dropped §2§]");

        let resumed = ContextFeature::new();
        let resumed_hook = resumed.request_context_hook(&ctx).unwrap();
        let third = resumed_hook.transform(messages).await.unwrap();
        assert_eq!(third, second, "a resumed session renders the same bytes");
    }
}
