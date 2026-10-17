//! Folding queued steers into the running turn (Claude Code's mid-turn
//! delivery): at each turn boundary, after the tool batch, the embedding's
//! queued steers that can join the run are admitted the way a steer sent to
//! a busy session is (skill and prompt-template expansion, the installed
//! features' prompt review and context row) and handed to the agent's
//! steering queue, which the loop drains before its next model call. The
//! run continues instead of ending and restarting on them. Steers that
//! cannot join (session commands, image-bearing prompts, injected rows: the
//! embedding decides) stay queued and still end the run at the boundary.
//!
//! A host notice (a finished background command, a watch event: the
//! embedding decides which) joins the same way as its own custom row, the
//! way Claude Code folds a task notification in after a tool batch. The host
//! wrote it, so it skips the expansion and review a user's text goes through.

use std::sync::{Arc, OnceLock, Weak};

use pa_agent::agent::{Agent, AgentMessageBatch};

use super::admission::FEATURE_CONTEXT_CUSTOM_TYPE;
use super::{SessionAgentMessage, session_message_to_loop, user_prompt_message};

/// One queued steer taken to join the running turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldedSteer {
    /// The prompt text as queued.
    pub text: String,
    /// The embedding's handle for [`SteeringFold::settled`].
    pub ticket: u64,
    /// A host notice's session custom row (`role: "custom"`): it joins as
    /// that row instead of a user prompt built from `text`.
    pub custom: Option<serde_json::Value>,
}

/// The embedding's queued steering, consulted at every turn boundary.
pub trait SteeringFold: Send + Sync {
    /// Take the queued steers that may join the running turn now, in queue
    /// order, stopping at the first that cannot.
    fn take(&self) -> Vec<FoldedSteer>;

    /// How a taken steer fared: `Ok` once it is in the agent's steering
    /// queue (it rides the next model call of this run), `Err` with the
    /// reason when the session refused it (an installed feature's review).
    fn settled(&self, steer: FoldedSteer, outcome: Result<(), String>);
}

/// What the boundary needs to admit a folded steer; filled in once the
/// session's agent exists.
pub(super) struct FoldAdmission {
    pub(super) agent: Weak<Agent>,
    pub(super) skills: Vec<crate::skills::Skill>,
    pub(super) prompt_templates: Vec<crate::skills::PromptTemplate>,
    pub(super) features: Vec<Arc<dyn crate::features::SessionFeature>>,
    pub(super) feature_context: Arc<crate::features::SessionFeatureContext>,
    pub(super) feature_notes: crate::features::FeatureNotes,
}

/// The admission slot the boundary hook reads (set after the agent is
/// built: the hook is part of the agent's own options).
pub(super) type FoldAdmissionSlot = Arc<OnceLock<FoldAdmission>>;

impl FoldAdmission {
    /// The row a host notice joins the run as: its own custom row, unreviewed.
    fn admit_notice(row: &serde_json::Value) -> Result<AgentMessageBatch, String> {
        let parsed = serde_json::from_value::<SessionAgentMessage>(row.clone())
            .map_err(|error| format!("the queued notice does not parse: {error}"))?;
        if !matches!(parsed, SessionAgentMessage::Custom(_)) {
            return Err("a queued notice must carry role \"custom\"".to_string());
        }
        session_message_to_loop(&parsed)
            .map(Into::into)
            .ok_or_else(|| "the queued notice has no loop form".to_string())
    }

    /// The rows one folded steer joins the run as, or the refusal.
    async fn admit(&self, text: &str) -> Result<AgentMessageBatch, String> {
        let (skill_expanded, _) = crate::skills::expand_skill_command(text, &self.skills);
        let normalized =
            crate::skills::expand_prompt_template(&skill_expanded, &self.prompt_templates);
        let mut rows = Vec::new();
        if !self.features.is_empty() {
            match crate::features::review_prompt(
                &self.features,
                &self.feature_context,
                &self.feature_notes,
                &normalized,
            )
            .await
            {
                crate::features::PromptDecision::Block(reason) => {
                    return Err(format!("The prompt was refused: {reason}"));
                }
                crate::features::PromptDecision::Continue {
                    context: Some(context),
                } => rows.extend(session_message_to_loop(&SessionAgentMessage::Custom(
                    super::plan_mode::hidden_row(FEATURE_CONTEXT_CUSTOM_TYPE, &context),
                ))),
                crate::features::PromptDecision::Continue { context: None } => {}
            }
        }
        rows.push(user_prompt_message(&normalized, &[]));
        Ok(if rows.len() == 1 {
            rows.remove(0).into()
        } else {
            rows.into()
        })
    }
}

/// Fold the embedding's joinable queued steers into the running turn: each
/// is admitted and steered into the agent, or settled as refused.
pub(super) async fn fold_queued_steers(fold: &dyn SteeringFold, slot: &FoldAdmissionSlot) {
    let Some(admission) = slot.get() else {
        return;
    };
    let Some(agent) = admission.agent.upgrade() else {
        return;
    };
    for steer in fold.take() {
        let admitted = match &steer.custom {
            Some(row) => FoldAdmission::admit_notice(row),
            None => admission.admit(&steer.text).await,
        };
        match admitted {
            Ok(batch) => {
                // A host row (a notice, an agent message) keeps driving the
                // session if the run is aborted before it is read; a user's
                // steer parks with the user's rows.
                if steer.custom.is_some() {
                    agent.steer_host(batch);
                } else {
                    agent.steer(batch);
                }
                fold.settled(steer, Ok(()));
            }
            Err(reason) => fold.settled(steer, Err(reason)),
        }
    }
}
