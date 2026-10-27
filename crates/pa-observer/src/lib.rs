//! Observer agents: a background observer model reads a read-only digest of
//! each turn of the session it observes and reports only when it has
//! something; the expected steady state is silence.
//!
//! Claude Code 2.1.296 (`t53z3rcv:Jbn`/`j`/`U`, tool `tkdb70xb`
//! `ObserverReport`, `7y9jhgf9:Vdo`; `CLAUDE_CODE_EXPERIMENTAL_OBSERVER_AGENTS`
//! and `tengu_observer_agents_enabled`) pairs an agent definition's `observer`
//! agent type with a background observer that receives a nonce-tagged
//! `<name-activity>` digest after each turn and reports through
//! `ObserverReport`, delivered to the observed agent as a one-way advisory.
//!
//! Here the observer is a tool-less model conversation the feature keeps
//! per session: each run end of the observed session sends the turn's
//! digest, the observer's reply is its report (`NO_REPORT` is silence), and
//! a report reaches the observed session as a feature notice (it joins the
//! running turn at its next boundary, or wakes an idle session). Off unless
//! `experimental.observer` (or `PRIME_AGENT_EXPERIMENTAL_OBSERVER_AGENTS`);
//! `experimental.observerModel` picks the model (else the session's),
//! `experimental.observerInstructions` extends each digest's postamble.
//! Sessions up to depth [`MAX_OBSERVED_DEPTH`] are observed (Claude Code's
//! subagent fan-out cap).

mod digest;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

pub use digest::{
    ActivityItem,
    DELIVERY_CHARS,
    ITEM_CHARS,
    OBSERVER_NOTICE_FEATURE,
    POSTAMBLE,
    activity_of,
    render_digest,
    report_notice,
    report_of,
    system_prompt,
};
use pa_agent::types::{
    AgentMessage,
    AssistantContent,
    AssistantMessage,
    Message,
    StopReason,
    TextContent,
    UserContent,
    UserMessage,
};
use pa_core::features::{
    FeatureNotice,
    SessionFeature,
    SessionFeatureContext,
    deliver_feature_notice,
};
use pa_core::session_engine::side_model::{SideModel, SideRequest};

/// Claude Code's observer fan-out depth cap (`G`).
pub const MAX_OBSERVED_DEPTH: u32 = 2;
/// The digest exchanges the observer keeps in its conversation.
pub const HISTORY_EXCHANGES: usize = 16;
/// The observed agent's name in digests and reports.
const OBSERVED_NAME: &str = "main";
/// The observer's name in reports.
const OBSERVER_NAME: &str = "observer";

/// Resolves a session's observer model (`provider/model-id` setting or
/// `None` for the session's model).
pub type ModelFactory =
    Arc<dyn Fn(&SessionFeatureContext, Option<&str>) -> SideModel + Send + Sync>;

/// The session's observer settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverSettings {
    pub model: Option<String>,
    pub instructions: Option<String>,
}

/// Reads whether a session is observed (production: the `experimental`
/// settings).
pub type SettingsReader =
    Arc<dyn Fn(&SessionFeatureContext) -> Option<ObserverSettings> + Send + Sync>;

struct ObservedSession {
    settings: ObserverSettings,
    model: SideModel,
    pending: Mutex<Vec<ActivityItem>>,
    /// The observer's conversation: (digest, reply) pairs, oldest first.
    history: tokio::sync::Mutex<Vec<(String, String)>>,
    /// The session's adoption counts (`observer_session`).
    tally: Tally,
}

/// What the observer did for one session (counts only).
#[derive(Default)]
struct Tally {
    digests: AtomicU64,
    reports: AtomicU64,
    failed: AtomicU64,
}

/// The adoption event a watched session sends when it ends.
pub const OBSERVER_SESSION_EVENT: &str = "observer_session";

/// The feature.
pub struct ObserverFeature {
    sessions: Mutex<HashMap<String, Arc<ObservedSession>>>,
    model_factory: ModelFactory,
    settings: SettingsReader,
    nonce: Arc<dyn Fn() -> String + Send + Sync>,
}

fn default_settings(context: &SessionFeatureContext) -> Option<ObserverSettings> {
    let experimental =
        pa_core::settings::SettingsManager::create(&context.cwd, &context.agent_dir).experimental();
    experimental.observer.then_some(ObserverSettings {
        model: experimental.observer_model,
        instructions: experimental.observer_instructions,
    })
}

fn default_model(context: &SessionFeatureContext, selector: Option<&str>) -> SideModel {
    SideModel::Selector {
        selector: selector.map_or_else(
            || format!("{}/{}", context.model.provider, context.model.id),
            str::to_string,
        ),
        agent_dir: context.agent_dir.clone(),
        cwd: context.cwd.clone(),
    }
}

fn random_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_nanos() & u128::from(u64::MAX)).unwrap_or_default()
        });
    format!(
        "{:012x}",
        (nanos ^ COUNTER.fetch_add(0x9e37_79b9, Ordering::Relaxed)) & 0xffff_ffff_ffff
    )
}

impl Default for ObserverFeature {
    fn default() -> Self {
        Self::new()
    }
}

impl ObserverFeature {
    /// The production feature: settings from `experimental`, the model
    /// through the registry.
    #[must_use]
    pub fn new() -> Self {
        Self::with(Arc::new(default_settings), Arc::new(default_model))
    }

    /// A feature with its settings and model injected (tests, embedders).
    #[must_use]
    pub fn with(settings: SettingsReader, model_factory: ModelFactory) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            model_factory,
            settings,
            nonce: Arc::new(random_nonce),
        }
    }

    /// Fix the digest nonce (tests).
    #[must_use]
    pub fn with_nonce(mut self, nonce: &'static str) -> Self {
        self.nonce = Arc::new(move || nonce.to_string());
        self
    }

    fn session(&self, session_id: &str) -> Option<Arc<ObservedSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id)
            .cloned()
    }
}

fn user_row(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn assistant_row(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        api: String::new(),
        provider: String::new(),
        model: String::new(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pa_agent::types::Usage::zero(),
        stop_reason: StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
        discarded_usage: None,
    })
}

/// Send one digest to the observer and deliver its report, if any. The
/// history lock serializes deliveries per session.
async fn deliver(session_id: String, observed: Arc<ObservedSession>, digest: String) {
    let mut history = observed.history.lock().await;
    let mut messages = Vec::with_capacity(history.len() * 2 + 1);
    for (sent, reply) in history.iter() {
        messages.push(user_row(sent));
        messages.push(assistant_row(reply));
    }
    messages.push(user_row(&digest));
    let reply = match observed
        .model
        .complete(SideRequest {
            system_prompt: system_prompt(OBSERVED_NAME),
            messages,
        })
        .await
    {
        Ok(reply) => reply,
        Err(error) => {
            observed.tally.failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(target: "pa_observer", "observer call failed: {error:#}");
            return;
        }
    };
    history.push((digest, reply.clone()));
    let excess = history.len().saturating_sub(HISTORY_EXCHANGES);
    history.drain(..excess);
    drop(history);
    if let Some(report) = report_of(&reply) {
        observed.tally.reports.fetch_add(1, Ordering::Relaxed);
        // The observer found something notable this turn — arm RAVO so its review
        // gate can decide whether it is worth persisting as memory. This is the
        // signal-driven refine trigger (replacing a hardcoded turn count): the
        // per-turn background agent drives the gated learning. No-op unless auto
        // refine is on; bounded by RAVO's cooldown.
        pa_core::session_engine::auto_refine_trigger::signal_observer_refine(&session_id);
        let delivered = deliver_feature_notice(
            &session_id,
            &FeatureNotice {
                feature: OBSERVER_NOTICE_FEATURE.to_string(),
                text: report_notice(OBSERVER_NAME, &report),
            },
        );
        if !delivered {
            tracing::debug!(target: "pa_observer", "observer report dropped: the session has no live sink");
        }
    }
}

impl SessionFeature for ObserverFeature {
    fn name(&self) -> &'static str {
        "observer"
    }

    fn on_session_start(&self, context: &Arc<SessionFeatureContext>, _history: &[AgentMessage]) {
        if context.rlm_depth > MAX_OBSERVED_DEPTH {
            return;
        }
        let Some(settings) = (self.settings)(context) else {
            return;
        };
        let model = (self.model_factory)(context, settings.model.as_deref());
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                context.session_id.clone(),
                Arc::new(ObservedSession {
                    settings,
                    model,
                    pending: Mutex::new(Vec::new()),
                    history: tokio::sync::Mutex::new(Vec::new()),
                    tally: Tally::default(),
                }),
            );
    }

    fn on_message_end(&self, context: &Arc<SessionFeatureContext>, message: &AgentMessage) {
        if let Some(observed) = self.session(&context.session_id) {
            observed
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(activity_of(message));
        }
    }

    fn on_agent_end(&self, context: &Arc<SessionFeatureContext>) {
        let Some(observed) = self.session(&context.session_id) else {
            return;
        };
        let mut items = std::mem::take(
            &mut *observed
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if items.is_empty() {
            return;
        }
        items.push(ActivityItem::TurnEnded {
            reason: "end".to_string(),
        });
        let digest = render_digest(
            OBSERVED_NAME,
            &items,
            &(self.nonce)(),
            observed.settings.instructions.as_deref(),
        );
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        observed.tally.digests.fetch_add(1, Ordering::Relaxed);
        runtime.spawn(deliver(context.session_id.clone(), observed, digest));
    }

    fn on_session_end(&self, context: &Arc<SessionFeatureContext>, _reason: &str) {
        let observed = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&context.session_id);
        if let (Some(observed), Some(telemetry)) = (observed, &context.telemetry) {
            let mut properties = pa_telemetry::Properties::new();
            for (key, count) in [
                ("digest_count", &observed.tally.digests),
                ("report_count", &observed.tally.reports),
                ("failed_count", &observed.tally.failed),
            ] {
                properties.set(key, count.load(Ordering::Relaxed).into());
            }
            telemetry.track(OBSERVER_SESSION_EVENT, &properties);
        }
    }
}

#[cfg(test)]
mod tests;
