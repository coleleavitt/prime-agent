//! The session feature: the trajectory's digest hook for every session, and
//! the recurrence filter `pa-cli` attaches to RAVO.

use std::sync::Arc;

use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::refinement::prompt_hook::HarnessPromptHook;

use crate::prompt::{InternalizedReminders, TrajectoryPromptHook};

/// The continual-learning gate (`pa-cli` installs it behind `feature =
/// "learning"`).
#[derive(Debug, Clone, Copy, Default)]
pub struct LearningFeature;

impl LearningFeature {
    /// Lever 2 for RAVO (`pa_ravo::RavoFeature::attach_recurrence_filter`).
    #[must_use]
    pub fn recurrence_filter(&self) -> Arc<dyn pa_ravo::RecurrenceFilter> {
        Arc::new(InternalizedReminders)
    }
}

impl SessionFeature for LearningFeature {
    fn name(&self) -> &'static str {
        "learning"
    }

    fn harness_prompt_hook(
        &self,
        context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn HarnessPromptHook>> {
        Some(Arc::new(TrajectoryPromptHook::new(&context.agent_dir)))
    }
}
