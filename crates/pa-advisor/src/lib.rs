//! Advisor escalation: the agent asks a stronger configured model for
//! guidance, with its conversation as the context.
//!
//! Claude Code 2.1.296 (`qwvy7ma3:bE`/`ITe`, `5bcjy17x`, server tool beta
//! `advisor-tool-2026-03-01`, `/advisor`; `tengu_sage_compass2` or
//! `CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL`) gives the main model an
//! `advisor()` tool backed by a stronger reviewer model that sees the whole
//! conversation. The beta is a server-side Anthropic tool; this harness
//! makes the call host-side instead, so any configured provider can advise.
//!
//! The bundled `advisor` skill's `advisor.ask(question)` sends the
//! `advisor.ask` host request; the feature renders the session's
//! conversation (collected from its finalized messages, newest kept within
//! [`TRANSCRIPT_CHARS`]) and asks the model `experimental.advisorModel` (or
//! `PRIME_AGENT_ADVISOR_MODEL`) names. Without that setting the request is
//! not registered and the skill is hidden.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use pa_agent::types::AgentMessage;
use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::{HostRequestHandlers, host_handler};
use pa_core::session_engine::side_model::{SideModel, SideRequest, render_transcript};
use serde_json::{Value, json};

/// The host request the skill sends.
pub const ADVISOR_REQUEST: &str = "advisor.ask";
/// The bundled skill (`skills/.features/advisor`).
pub const ADVISOR_SKILL: &str = "advisor";
/// The transcript the advisor reads (characters, newest kept).
pub const TRANSCRIPT_CHARS: usize = 120_000;
/// The longest question accepted.
pub const MAX_QUESTION_CHARS: usize = 8_000;
/// The finalized messages kept per session (the transcript's source).
const KEPT_MESSAGES: usize = 400;

/// The advisor's system prompt.
pub const ADVISOR_SYSTEM_PROMPT: &str = "You are the advisor: a stronger reviewer model that an agent consults while it works. You are given the agent's conversation so far (the task, its code cells and tool results, its replies) and the agent's question. You do not act: give guidance the agent can act on. Be concrete and brief: name the decision, the likely cause, or the risk, the evidence for it from the transcript, and the next step. Say when the evidence points against the agent's current approach. If the question cannot be answered from the transcript, say what the agent should check first.";

/// The user text of one consultation.
#[must_use]
pub fn consultation_prompt(transcript: &str, question: &str) -> String {
    format!("<conversation>\n{transcript}\n</conversation>\n\nThe agent asks:\n{question}")
}

/// Resolves a session's advisor model from its selector.
pub type ModelFactory = Arc<dyn Fn(&SessionFeatureContext, &str) -> SideModel + Send + Sync>;
/// Reads the session's advisor selector (production: the `experimental`
/// settings); `None` leaves the advisor off.
pub type SelectorReader = Arc<dyn Fn(&SessionFeatureContext) -> Option<String> + Send + Sync>;

type Transcripts = Arc<Mutex<HashMap<String, Vec<AgentMessage>>>>;

/// The feature.
pub struct AdvisorFeature {
    transcripts: Transcripts,
    selector: SelectorReader,
    model_factory: ModelFactory,
}

fn default_selector(context: &SessionFeatureContext) -> Option<String> {
    pa_core::settings::SettingsManager::create(&context.cwd, &context.agent_dir)
        .experimental()
        .advisor_model
}

fn default_model(context: &SessionFeatureContext, selector: &str) -> SideModel {
    SideModel::Selector {
        selector: selector.to_string(),
        agent_dir: context.agent_dir.clone(),
        cwd: context.cwd.clone(),
    }
}

impl Default for AdvisorFeature {
    fn default() -> Self {
        Self::new()
    }
}

impl AdvisorFeature {
    /// The production feature: the selector from `experimental`, the model
    /// through the registry.
    #[must_use]
    pub fn new() -> Self {
        Self::with(Arc::new(default_selector), Arc::new(default_model))
    }

    /// A feature with its selector and model injected (tests, embedders).
    #[must_use]
    pub fn with(selector: SelectorReader, model_factory: ModelFactory) -> Self {
        Self {
            transcripts: Arc::new(Mutex::new(HashMap::new())),
            selector,
            model_factory,
        }
    }
}

fn validated_question(payload: &Value) -> anyhow::Result<String> {
    let question = payload
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|question| !question.is_empty())
        .ok_or_else(|| anyhow::anyhow!("advisor.ask question must be a non-empty string"))?;
    if question.chars().count() > MAX_QUESTION_CHARS {
        anyhow::bail!("advisor.ask question must be at most {MAX_QUESTION_CHARS} characters");
    }
    Ok(question.to_string())
}

/// The adoption event (`advisor_ask`): outcome and duration only.
fn track_ask(
    telemetry: Option<&pa_core::features::FeatureTelemetry>,
    outcome: &'static str,
    started: std::time::Instant,
) {
    let Some(telemetry) = telemetry else {
        return;
    };
    let mut properties = pa_telemetry::Properties::new();
    properties.set("outcome", Value::from(outcome));
    properties.set(
        "duration_ms",
        Value::from(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
    );
    telemetry.track(ADVISOR_ASK_EVENT, &properties);
}

/// The adoption event one consultation sends.
pub const ADVISOR_ASK_EVENT: &str = "advisor_ask";

impl SessionFeature for AdvisorFeature {
    fn name(&self) -> &'static str {
        "advisor"
    }

    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        let Some(selector) = (self.selector)(context) else {
            return;
        };
        let model = (self.model_factory)(context, &selector);
        let transcripts = Arc::clone(&self.transcripts);
        let session_id = context.session_id.clone();
        let telemetry = context.telemetry.clone();
        handlers.register(
            ADVISOR_REQUEST,
            host_handler(move |payload| {
                let model = model.clone();
                let transcripts = Arc::clone(&transcripts);
                let session_id = session_id.clone();
                let selector = selector.clone();
                let telemetry = telemetry.clone();
                Box::pin(async move {
                    let started = std::time::Instant::now();
                    let question = match validated_question(&payload.data) {
                        Ok(question) => question,
                        Err(error) => {
                            track_ask(telemetry.as_ref(), "invalid", started);
                            return Err(error);
                        }
                    };
                    let transcript = {
                        let transcripts =
                            transcripts.lock().unwrap_or_else(PoisonError::into_inner);
                        render_transcript(
                            transcripts.get(&session_id).map_or(&[][..], Vec::as_slice),
                            TRANSCRIPT_CHARS,
                        )
                    };
                    let advice = model
                        .complete(SideRequest::single(
                            ADVISOR_SYSTEM_PROMPT,
                            consultation_prompt(&transcript, &question),
                        ))
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!("the advisor ({selector}) failed: {error:#}")
                        });
                    track_ask(
                        telemetry.as_ref(),
                        if advice.is_ok() { "ok" } else { "error" },
                        started,
                    );
                    Ok(json!({ "advice": advice?, "model": selector }))
                })
            }),
        );
    }

    fn on_session_start(&self, context: &Arc<SessionFeatureContext>, history: &[AgentMessage]) {
        if (self.selector)(context).is_none() {
            return;
        }
        let kept = history.len().saturating_sub(KEPT_MESSAGES);
        self.transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(context.session_id.clone(), history[kept..].to_vec());
    }

    fn on_message_end(&self, context: &Arc<SessionFeatureContext>, message: &AgentMessage) {
        let mut transcripts = self
            .transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(messages) = transcripts.get_mut(&context.session_id) {
            messages.push(message.clone());
            let excess = messages.len().saturating_sub(KEPT_MESSAGES);
            messages.drain(..excess);
        }
    }

    fn on_session_end(&self, context: &Arc<SessionFeatureContext>, _reason: &str) {
        self.transcripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&context.session_id);
    }

    fn bundled_skills(&self) -> Vec<&'static str> {
        vec![ADVISOR_SKILL]
    }

    /// The `advisor` skill only where an advisor model is configured.
    fn session_skill_visible(&self, context: &SessionFeatureContext, skill_name: &str) -> bool {
        skill_name != ADVISOR_SKILL || (self.selector)(context).is_some()
    }
}

#[cfg(test)]
mod tests;
