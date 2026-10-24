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
                SideUxRequest::Recap => {
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
            }
        });
        outcome.unwrap_or_else(|error| SideUxOutcome::Failed(format!("{error:#}")))
    }
}

fn text_or_nothing(text: Option<String>) -> SideUxOutcome {
    text.map_or(SideUxOutcome::Nothing, SideUxOutcome::Text)
}
