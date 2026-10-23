//! The installed features at the turn boundary: after a tool batch their
//! context rides the next model request
//! ([`crate::features::SessionFeature::after_tool_batch`]); after a final
//! turn (no tool calls, nothing queued to continue the run) they may keep
//! the agent going ([`crate::features::SessionFeature::before_agent_stop`]).
//! Both deliver through the agent's steering queue, which the loop drains
//! right after the boundary, so the rows join the running run the way a
//! folded steer does (`steering_fold`).

use pa_agent::agent::Agent;
use pa_agent::types::{AgentMessage, AssistantContent, ShouldStopAfterTurnContext, StopReason};

use super::admission::FEATURE_CONTEXT_CUSTOM_TYPE;
use super::steering_fold::FoldAdmissionSlot;
use super::{SessionAgentMessage, session_message_to_loop};
use crate::features::{
    AgentStopObservation,
    MAX_FEATURE_CONTINUATIONS_PER_RUN,
    ToolBatchCall,
    ToolBatchObservation,
};

/// The custom type of the hidden row a feature continuation (a `Stop` that
/// keeps the agent going) reaches the model as.
pub const FEATURE_CONTINUATION_CUSTOM_TYPE: &str = "feature_continuation";

pub(super) fn steer_hidden_row(agent: &Agent, custom_type: &str, text: &str) {
    if let Some(row) = session_message_to_loop(&SessionAgentMessage::Custom(
        super::plan_mode::hidden_row(custom_type, text),
    )) {
        agent.steer(row);
    }
}

fn is_continuation_row(message: &AgentMessage) -> bool {
    matches!(message, AgentMessage::Custom(row)
        if row.payload.get("customType").and_then(serde_json::Value::as_str)
            == Some(FEATURE_CONTINUATION_CUSTOM_TYPE))
}

fn assistant_text(turn: &ShouldStopAfterTurnContext) -> String {
    turn.message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run the features' boundary seams for the turn that just ended; called
/// only when the run is not stopping here (a queued steer the embedding
/// holds already stopped it).
pub(super) async fn after_turn(slot: &FoldAdmissionSlot, turn: &ShouldStopAfterTurnContext) {
    let Some(admission) = slot.get() else {
        return;
    };
    if admission.features.is_empty() {
        return;
    }
    let Some(agent) = admission.agent.upgrade() else {
        return;
    };
    if !turn.tool_results.is_empty() {
        let batch = ToolBatchObservation {
            tool_calls: turn
                .tool_results
                .iter()
                .map(|result| ToolBatchCall {
                    tool_call_id: result.tool_call_id.clone(),
                    tool_name: result.tool_name.clone(),
                })
                .collect(),
        };
        if let Some(context) = crate::features::review_tool_batch(
            &admission.features,
            &admission.feature_context,
            &batch,
        )
        .await
        {
            steer_hidden_row(&agent, FEATURE_CONTEXT_CUSTOM_TYPE, &context);
        }
        return;
    }
    // Only a natural end stops the agent: a cut-off reply continues (or
    // ends in error), and queued input continues the run.
    if turn.message.stop_reason != StopReason::Stop || agent.has_queued_messages() {
        return;
    }
    let continuations = turn
        .new_messages
        .iter()
        .filter(|message| is_continuation_row(message))
        .count();
    let stop = AgentStopObservation {
        last_assistant_message: assistant_text(turn),
        continued: continuations > 0,
    };
    let Some(text) =
        crate::features::review_agent_stop(&admission.features, &admission.feature_context, &stop)
            .await
    else {
        return;
    };
    if continuations >= MAX_FEATURE_CONTINUATIONS_PER_RUN {
        tracing::warn!(
            target: "pa_core::features",
            continuations,
            "a feature asked to continue past the per-run cap; the agent stops"
        );
        return;
    }
    steer_hidden_row(&agent, FEATURE_CONTINUATION_CUSTOM_TYPE, &text);
}
