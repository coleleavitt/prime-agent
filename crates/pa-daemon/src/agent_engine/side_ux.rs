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
            SideUxRequest::SessionName | SideUxRequest::Recap => true,
        };
        if !enabled {
            return SideUxOutcome::Disabled;
        }
        let agent = match self.session_agent(&model) {
            Ok(agent) => agent,
            Err(error) => return SideUxOutcome::Failed(error.to_string()),
        };
        let stream_fn = {
            let guard = self.session.blocking_lock();
            guard
                .as_deref()
                .and_then(|engine| engine.session.side_question_stream_fn())
        };
        let Some(stream_fn) = stream_fn else {
            return SideUxOutcome::Failed("Select a model first".to_string());
        };
        let selector = settings.side_ux_model();
        let cwd = self.cwd();
        let agent_dir = self.config.agent_dir.clone();
        let outcome = self.runtime.block_on(async {
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
                    side_ux::generate_title(&side, &first_message)
                        .await
                        .map(text_or_nothing)
                }
                SideUxRequest::SessionName => {
                    let side = side().await;
                    let messages = agent.state().await.messages;
                    side_ux::generate_session_name(&side, &messages)
                        .await
                        .map(text_or_nothing)
                }
                SideUxRequest::Recap | SideUxRequest::AutoRecap => {
                    side_ux::recap(&agent, &stream_fn)
                        .await
                        .map(|outcome| match outcome {
                            side_ux::RecapOutcome::Recap { text, .. } => SideUxOutcome::Text(text),
                            side_ux::RecapOutcome::NoTurn => SideUxOutcome::Nothing,
                        })
                }
                SideUxRequest::PromptSuggestion => side_ux::prompt_suggestion(&agent, &stream_fn)
                    .await
                    .map(text_or_nothing),
                SideUxRequest::ToolUseLabel => {
                    let side: SideModel = side().await;
                    let messages = agent.state().await.messages;
                    let (calls, last_assistant_text) = collect_tool_use_label_inputs(&messages);
                    if calls.is_empty() {
                        Ok(SideUxOutcome::Nothing)
                    } else {
                        side_ux::generate_tool_use_label(&side, &calls, &last_assistant_text)
                            .await
                            .map(text_or_nothing)
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
