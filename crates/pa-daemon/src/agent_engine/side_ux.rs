//! The side-model UX calls on the real engine ([`SessionEngine::run_side_ux`]):
//! Claude Code's generated session title and `/rename` name on the side
//! model (`sideUxModel`, else `auxiliaryModel`, else the session's own), and
//! the away summary and prompt suggestion over the session's own
//! conversation (`pa_core::session_engine::side_ux`).
//!
//! A scripted (faux) session answers `Unsupported`: its model replies are a
//! fixed script, and a side call would consume the turns the session's own
//! requests are owed.
//!
//! [`SessionEngine::run_side_ux`]: crate::engine::SessionEngine::run_side_ux

use pa_core::session_engine::side_model::{SideModel, side_model_for};
use pa_core::session_engine::side_ux;

use super::AgentSessionEngine;
use crate::engine::{SideUxOutcome, SideUxRequest};

/// The provider api a scripted session runs on.
const FAUX_API: &str = "faux";

impl AgentSessionEngine {
    pub(super) fn side_ux(&self, request: SideUxRequest) -> SideUxOutcome {
        let model = match self.session_model() {
            Ok(model) => model,
            Err(error) => return SideUxOutcome::Failed(error.to_string()),
        };
        if model.api == FAUX_API {
            return SideUxOutcome::Unsupported;
        }
        let settings =
            pa_core::settings::SettingsManager::create(self.cwd(), &self.config.agent_dir);
        let switches = settings.side_ux();
        let enabled = match &request {
            SideUxRequest::SessionTitle { .. } => switches.session_titles,
            SideUxRequest::PromptSuggestion => switches.prompt_suggestions,
            SideUxRequest::AutoRecap => switches.away_summary,
            SideUxRequest::ToolUseLabel => switches.tool_use_labels,
            // On demand: `/rename` and `/recap` run whenever the user asks.
            // `MemoryExtract` likewise bypasses the side-UX switches — its
            // own engine-side gate is `per_turn_memory_enabled` on the
            // session, checked by `auto_refine_per_turn` itself; we let
            // the dispatch through here so the skip reason is logged.
            SideUxRequest::SessionName
            | SideUxRequest::Recap
            | SideUxRequest::MemoryExtract { .. } => true,
        };
        if !enabled {
            return SideUxOutcome::Disabled;
        }
        let agent = match self.session_agent(&model) {
            Ok(agent) => agent,
            Err(error) => return SideUxOutcome::Failed(error.to_string()),
        };
        let (stream_fn, session) = {
            let guard = self.session.blocking_lock();
            let engine = guard.as_deref();
            (
                engine.and_then(|engine| engine.session.side_question_stream_fn()),
                engine.map(|engine| engine.session.shared_persistence()),
            )
        };
        let (Some(stream_fn), Some(session)) = (stream_fn, session) else {
            return SideUxOutcome::Failed("Select a model first".to_string());
        };
        let selector = settings.side_ux_model();
        let cwd = self.cwd();
        let agent_dir = self.config.agent_dir.clone();
        // For `MemoryExtract`: the refinement run reuses the session's
        // own model + api key, same as compaction's auto-refine path.
        let (_, session_api_key) = self.resolve_request_target(model.clone());
        let outcome = self.runtime.block_on(async {
            // The session the side calls bill.
            let session_id = session.lock().await.get_session_id().to_string();
            let side = || async {
                let session_model = agent.state().await.model;
                side_model_for(
                    selector.as_deref(),
                    &stream_fn,
                    &session_model,
                    &agent_dir,
                    &cwd,
                )
            };
            match request {
                SideUxRequest::SessionTitle { first_message } => {
                    let side: SideModel = side().await;
                    side_ux::generate_title(&side, &session_id, &first_message)
                        .await
                        .map(text_or_nothing)
                }
                SideUxRequest::SessionName => {
                    let side = side().await;
                    let messages = agent.state().await.messages;
                    side_ux::generate_session_name(&side, &session_id, &messages)
                        .await
                        .map(text_or_nothing)
                }
                SideUxRequest::Recap | SideUxRequest::AutoRecap => {
                    side_ux::recap(&agent, &stream_fn, &session_id)
                        .await
                        .map(|outcome| match outcome {
                            side_ux::RecapOutcome::Recap { text, .. } => SideUxOutcome::Text(text),
                            side_ux::RecapOutcome::NoTurn => SideUxOutcome::Nothing,
                        })
                }
                SideUxRequest::PromptSuggestion => {
                    side_ux::prompt_suggestion(&agent, &stream_fn, &session_id)
                        .await
                        .map(text_or_nothing)
                }
                SideUxRequest::ToolUseLabel => {
                    let side: SideModel = side().await;
                    let messages = agent.state().await.messages;
                    let (calls, last_assistant_text) = collect_tool_use_label_inputs(&messages);
                    if calls.is_empty() {
                        Ok(SideUxOutcome::Nothing)
                    } else {
                        side_ux::generate_tool_use_label(
                            &side,
                            &session_id,
                            &calls,
                            &last_assistant_text,
                        )
                        .await
                        .map(text_or_nothing)
                    }
                }
                SideUxRequest::MemoryExtract { user_message_count } => {
                    // The per-turn memory extractor runs the refinement
                    // pipeline on the current transcript (review + apply)
                    // through `auto_refine_per_turn`. A skip or decline
                    // leaves no visible row; an applied run returns the
                    // serialized `RefinementResult` so the worker side can
                    // inject a `refinement_outcome` custom row — the same
                    // in-stream primitive the compaction auto-refine boundary
                    // uses (`agent_engine/turn/boundary.rs`), matching
                    // Claude Code's `appendSystemMessage` placement.
                    let session_guard = self.session.lock().await;
                    let Some(engine) = session_guard.as_deref() else {
                        return Ok(SideUxOutcome::Nothing);
                    };
                    let outcome = engine
                        .session
                        .auto_refine_per_turn(
                            &model,
                            session_api_key.clone(),
                            self.config.global_harness_dir(),
                            user_message_count,
                        )
                        .await;
                    drop(session_guard);
                    match outcome {
                        Err(error) => Err(error),
                        Ok(pa_core::session_engine::refine::PerTurnRunOutcome::Skipped(reason)) => {
                            tracing::info!(
                                target: "pa_daemon::memory_extract",
                                reason = reason.log_tail(),
                                "[extractMemories] skipping",
                            );
                            Ok(SideUxOutcome::Nothing)
                        }
                        Ok(pa_core::session_engine::refine::PerTurnRunOutcome::Declined(
                            rationale,
                        )) => {
                            tracing::info!(
                                target: "pa_daemon::memory_extract",
                                rationale = %rationale,
                                "[extractMemories] review declined",
                            );
                            Ok(SideUxOutcome::RefinementDeclined {
                                reason:
                                    pa_core::session_engine::refine::AUTO_REFINE_PER_TURN_REASON
                                        .to_string(),
                                rationale,
                            })
                        }
                        Ok(pa_core::session_engine::refine::PerTurnRunOutcome::Ran(result)) => {
                            let applied_count = result
                                .applied_edits
                                .iter()
                                .filter(|edit| edit.applied)
                                .count();
                            tracing::info!(
                                target: "pa_daemon::memory_extract",
                                count = applied_count,
                                "[extractMemories] refinement applied",
                            );
                            if applied_count == 0 {
                                return Ok(SideUxOutcome::Nothing);
                            }
                            match serde_json::to_string(&*result) {
                                Ok(result_json) => {
                                    Ok(SideUxOutcome::RefinementApplied { result_json })
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        target: "pa_daemon::memory_extract",
                                        error = %error,
                                        "[extractMemories] could not serialize result",
                                    );
                                    Ok(SideUxOutcome::Nothing)
                                }
                            }
                        }
                    }
                }
            }
        });
        outcome.unwrap_or_else(|error| SideUxOutcome::Failed(format!("{error:#}")))
    }
}

fn text_or_nothing(text: Option<String>) -> SideUxOutcome {
    text.map_or(SideUxOutcome::Nothing, SideUxOutcome::Text)
}

/// Gather the last tool batch from `messages` and the last assistant text
/// as [`side_ux::generate_tool_use_label`] reads them.
///
/// The last batch is the trailing run of `ToolResult` messages (CC pairs
/// each tool call with a result; prime-agent carries the same shape).
/// Per-call previews read the tool input and output as plain JSON text,
/// clipped to [`side_ux::TOOL_USE_LABEL_CALL_CLIP`] graphemes inside the
/// generator itself; the last assistant text is the newest assistant
/// message's visible text (empty when none).
fn collect_tool_use_label_inputs(
    messages: &[pa_agent::types::AgentMessage],
) -> (Vec<side_ux::ToolUseLabelCall>, String) {
    use pa_agent::types::{AgentMessage, AssistantContent, Message};
    let mut calls: Vec<side_ux::ToolUseLabelCall> = Vec::new();
    // The trailing tool-result run represents the last batch. Walk back
    // until we hit the first non-tool-result, collecting results in order.
    let mut tool_results: Vec<&pa_agent::types::ToolResultMessage> = Vec::new();
    for message in messages.iter().rev() {
        match message {
            AgentMessage::Standard(Message::ToolResult(result)) => tool_results.push(result),
            _ => break,
        }
    }
    tool_results.reverse();
    // Map result.tool_call_id → the tool call's input (if we can find it
    // on the preceding assistant message) and name.
    let last_assistant = messages.iter().rev().find_map(|message| match message {
        AgentMessage::Standard(Message::Assistant(assistant)) => Some(assistant),
        _ => None,
    });
    let last_assistant_text: String = last_assistant
        .map(|assistant| {
            assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Text(text) => Some(text.text.as_str()),
                    AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let tool_calls_by_id: std::collections::HashMap<&str, &pa_agent::types::ToolCall> =
        last_assistant
            .map(|assistant| {
                assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(call) => Some((call.id.as_str(), call)),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
    for result in tool_results {
        let call = tool_calls_by_id.get(result.tool_call_id.as_str());
        let tool_name = call.map_or_else(|| result.tool_name.clone(), |call| call.name.clone());
        let input_preview = call
            .map(|call| serde_json::to_string(&call.arguments).unwrap_or_default())
            .unwrap_or_default();
        let output_preview = serde_json::to_string(&result.content).unwrap_or_default();
        calls.push(side_ux::ToolUseLabelCall {
            tool_name,
            input_preview,
            output_preview,
        });
    }
    (calls, last_assistant_text)
}
