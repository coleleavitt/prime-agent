//! The worker's dropped-tool-call recovery hook (upstream #2530, TS
//! `_installAgentToolIntentRecoveryHook`): a reply that reported a tool
//! call and delivered none retries once, inside the same run. The agent
//! loop owns the once-per-run bound and the tool-choice override; this seam
//! owns the session policy — interactive runs only (autonomous and goal
//! runs continue on their own), and never while other work holds the
//! boundary.

use std::sync::Arc;

use pa_types::sync::MutexExt;

use crate::agent_engine::AgentSessionEngine;

impl AgentSessionEngine {
    /// Install the recovery hook on the built session's agent. The engine
    /// holds itself weakly ([`Self::register_arc`]); an engine without a
    /// registered arc installs nothing.
    pub(crate) fn install_tool_intent_recovery_hook_on(
        &self,
        agent: &Arc<pa_agent::agent::Agent>,
        telemetry: Option<Arc<pa_core::session_engine::telemetry::SessionTelemetry>>,
    ) {
        let weak = self.self_weak.lock_or_recover().clone();
        let Some(weak) = weak else {
            return;
        };
        agent.set_tool_intent_recovery_hook(Some(Arc::new(move |context| {
            let weak = weak.clone();
            let telemetry = telemetry.clone();
            Box::pin(async move {
                let Some(engine) = weak.upgrade() else {
                    return Ok(None);
                };
                if !engine.tool_intent_recovery_allowed().await {
                    return Ok(None);
                }
                let model = engine.session_model().ok();
                if !pa_core::session_engine::tool_intent_recovery::is_dropped_tool_call_stop(
                    &context.message,
                    model.as_ref(),
                ) {
                    return Ok(None);
                }
                if let Some(telemetry) = &telemetry {
                    telemetry.note_adoption(
                        pa_core::session_engine::telemetry::SessionAdoption::ToolIntentRecovery,
                    );
                }
                Ok(Some(
                    pa_core::session_engine::tool_intent_recovery::tool_intent_recovery_row(
                        pa_core::autonomous::now_millis(),
                    ),
                ))
            })
                as pa_agent::BoxFut<'static, anyhow::Result<Option<pa_agent::types::AgentMessage>>>
        })));
    }

    /// The session-side gate (TS: no queued actions, an idle goal, no
    /// autonomous mode, no unsettled RLM work, no live background bash).
    pub(crate) async fn tool_intent_recovery_allowed(&self) -> bool {
        if self.session_is_closed() || self.session_input_queued() {
            return false;
        }
        let goal = self.goal_runtime.lock_or_recover().clone();
        if let Some(handles) = goal {
            if handles.driver.lock().await.state().status != pa_types::goal::GoalStatus::Idle {
                return false;
            }
        }
        if self.autonomous.lock().await.enabled {
            return false;
        }
        !(self.has_unsettled_rlm_work().await || self.has_live_background_bash_handles())
    }
}
