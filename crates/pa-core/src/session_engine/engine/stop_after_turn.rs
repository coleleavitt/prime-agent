//! The session's after-turn boundary (the loop's `should_stop_after_turn`): queued steers
//! fold into the run, the embedding decides whether the run stops, and a run that goes on
//! past the boundary carries the per-turn riders (usage-limit and goal checks, nested
//! instructions, dynamic skills, the features, brief-mode enforcement, the context rows).

use std::sync::Arc;

use pa_agent::agent_loop::ShouldStopAfterTurnFn;
use pa_agent::types::ShouldStopAfterTurnContext;

use crate::session_engine::steering_fold::{FoldAdmissionSlot, SteeringFold, fold_queued_steers};

/// What the boundary reads; built once per session, shared by every turn's call.
pub(super) struct StopAfterTurn {
    pub(super) fold: Option<Arc<dyn SteeringFold>>,
    pub(super) fold_slot: FoldAdmissionSlot,
    /// Whether a queued steer is waiting.
    pub(super) steering: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// The embedding's own stop decision.
    pub(super) boundary: Option<ShouldStopAfterTurnFn>,
    pub(super) usage_limit: Option<Arc<crate::session_engine::usage_limit::UsageLimitMonitor>>,
    pub(super) goal_evaluator: Option<Arc<crate::session_engine::goal_evaluator::GoalEvaluator>>,
    pub(super) nested: Option<Arc<crate::resources::instructions::NestedInstructions>>,
    pub(super) dynamic_skills: Option<Arc<crate::skills::dynamic::DynamicSkills>>,
    pub(super) known_skills: Arc<Vec<crate::skills::Skill>>,
    pub(super) context_rows: Arc<crate::session_engine::context_rows::SessionContextRows>,
    pub(super) modes_boundary: crate::session_engine::modes::ModesBoundary,
}

impl StopAfterTurn {
    /// The loop's hook over this boundary.
    pub(super) fn into_hook(self) -> ShouldStopAfterTurnFn {
        let boundary = Arc::new(self);
        Arc::new(move |context| {
            let boundary = Arc::clone(&boundary);
            Box::pin(async move { boundary.run(context).await })
        })
    }

    async fn run(&self, context: ShouldStopAfterTurnContext) -> anyhow::Result<bool> {
        // Queued steers that can join this run go into the agent's
        // steering queue now (the loop drains it before its next
        // model call); only the ones that cannot still end the run.
        if let Some(fold) = &self.fold {
            fold_queued_steers(fold.as_ref(), &self.fold_slot).await;
        }
        // A queued steer ends the run only once the agent holds no
        // rows of its own: rows folded in above ride this run's
        // next turn. Ending the run under them would leave them to
        // the idle pump, whose run no embedding owns.
        let steer_ends_run = self.steering.as_ref().is_some_and(|probe| probe())
            && !self
                .fold_slot
                .get()
                .and_then(|admission| admission.agent.upgrade())
                .is_some_and(|agent| agent.has_queued_messages());
        if steer_ends_run {
            return Ok(true);
        }
        // The installed features see the boundary only when the
        // run goes on past it (the copy is made only for them).
        let feature_turn = (!crate::features::installed().is_empty()).then(|| context.clone());
        // The usage-limit monitor and the goal evaluator read the
        // boundary only when the run goes on past it, like the features.
        let experimental_turn =
            (self.usage_limit.is_some() || self.goal_evaluator.is_some()).then(|| context.clone());
        // The files this batch touched (nested instruction discovery).
        let mut touched = if self.nested.is_some() || self.dynamic_skills.is_some() {
            crate::session_engine::instruction_boundary::touched_paths(&context.tool_results)
        } else {
            Vec::new()
        };
        // A coordinator also loads what its workers loaded.
        if self.nested.is_some() {
            self.modes_boundary
                .propagate_nested(&context, &mut touched)
                .await;
        }
        let brief_enforcement = self.modes_boundary.brief_enforcement(&context);
        // The nested files the conversation still carries (a
        // compaction may have summarized some away).
        let live = if touched.is_empty() {
            Vec::new()
        } else {
            crate::session_engine::instruction_boundary::delivered_paths(&context.context.messages)
        };
        // The context and reminder rows ride a run that goes on
        // past a tool batch.
        let reminder_turn = (!context.tool_results.is_empty()).then(|| context.clone());
        let stop = match &self.boundary {
            Some(hook) => hook(context).await?,
            None => false,
        };
        if stop {
            return Ok(true);
        }
        let slot = &self.fold_slot;
        if let Some(turn) = experimental_turn {
            if let Some(monitor) = &self.usage_limit {
                crate::session_engine::usage_limit::after_turn(monitor, slot, &turn);
            }
            if let Some(evaluator) = &self.goal_evaluator {
                crate::session_engine::goal_evaluator::after_turn(evaluator, slot, &turn).await;
            }
        }
        if let Some(nested) = &self.nested {
            crate::session_engine::instruction_boundary::after_turn(slot, nested, &touched, &live);
        }
        if let Some(dynamic) = &self.dynamic_skills {
            crate::session_engine::skill_boundary::after_turn(
                slot,
                dynamic,
                &self.known_skills,
                touched.clone(),
            )
            .await;
        }
        if let Some(turn) = feature_turn {
            crate::session_engine::feature_boundary::after_turn(slot, &turn).await;
        }
        if let Some(text) = brief_enforcement {
            crate::session_engine::brief_mode::steer_enforcement(slot, &text);
        }
        if let Some(turn) = reminder_turn {
            let agent = slot.get().and_then(|admission| admission.agent.upgrade());
            if let Some(agent) = agent {
                crate::session_engine::context_rows::after_turn(&self.context_rows, &agent, &turn)
                    .await;
            }
        }
        Ok(false)
    }
}
