//! Session context and reminders as hidden conversation rows (Claude Code's
//! context attachments and per-turn reminders), so the system prompt stays
//! byte-stable for the provider's prompt cache.
//!
//! At each admitted turn the session's current context
//! ([`crate::prompts::session_context`]) is compared with what the
//! conversation already announced: with no full `session_context` row in the
//! live context (a fresh session, a resumed one from before this row
//! existed, a compaction that summarized it away) the full row rides ahead
//! of the prompt; otherwise only a `session_context_update` row with what
//! changed, or nothing. Every row carries the fingerprint of what it
//! announced, so the comparison needs no state beyond the conversation.
//!
//! Between a tool batch and the run's next model call the boundary adds the
//! date change (a run that crosses midnight) and the reminder row
//! ([`super::reminders`]); the rows join the run through the agent's
//! steering queue, the way a folded steer does.

use std::sync::Arc;

use pa_agent::types::AgentMessage;
use pa_types::session::CustomMessage;

use super::reminders::{ReminderPoint, Reminders};
use super::{SessionAgentMessage, session_message_to_loop};
use crate::prompts::session_context::{
    AMBIENT_CONTEXT_NOTE,
    ContextFingerprint,
    SessionContext,
    render_date_change,
    system_reminder,
};

/// The custom type of the first-turn context row.
pub const SESSION_CONTEXT_CUSTOM_TYPE: &str = "session_context";
/// The custom type of a context delta row.
pub const SESSION_CONTEXT_UPDATE_CUSTOM_TYPE: &str = "session_context_update";

/// Builds the session's current context. Blocking (git probes, instruction
/// file reads): called off the async runtime.
pub type SessionContextSource = Arc<dyn Fn() -> SessionContext + Send + Sync>;

/// What the live conversation already announced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Announced {
    /// A full `session_context` row is in the context.
    pub full: bool,
    /// The newest row's fingerprint (full or update).
    pub latest: Option<ContextFingerprint>,
}

/// Read what the context rows in `messages` announced.
#[must_use]
pub fn announced(messages: &[AgentMessage]) -> Announced {
    let mut out = Announced::default();
    for message in messages {
        let AgentMessage::Custom(custom) = message else {
            continue;
        };
        let custom_type = custom
            .payload
            .get("customType")
            .and_then(serde_json::Value::as_str);
        let full = match custom_type {
            Some(SESSION_CONTEXT_CUSTOM_TYPE) => true,
            Some(SESSION_CONTEXT_UPDATE_CUSTOM_TYPE) => false,
            _ => continue,
        };
        let Some(fingerprint) = custom
            .payload
            .get("details")
            .and_then(|details| details.get("fingerprint"))
            .cloned()
            .and_then(|value| serde_json::from_value::<ContextFingerprint>(value).ok())
        else {
            continue;
        };
        out.full |= full;
        out.latest = Some(fingerprint);
    }
    out
}

fn context_row(custom_type: &str, text: &str, fingerprint: &ContextFingerprint) -> CustomMessage {
    let mut row = super::plan_mode::hidden_row(custom_type, text);
    row.details = Some(serde_json::json!({ "fingerprint": fingerprint }));
    row
}

/// The row that brings the model up to `current`, or `None` when the
/// conversation already says it all.
#[must_use]
pub fn context_row_for(current: &SessionContext, announced: &Announced) -> Option<CustomMessage> {
    match (&announced.latest, announced.full) {
        (Some(latest), true) => current.render_delta(latest).map(|text| {
            context_row(
                SESSION_CONTEXT_UPDATE_CUSTOM_TYPE,
                &text,
                &current.fingerprint(),
            )
        }),
        _ => Some(context_row(
            SESSION_CONTEXT_CUSTOM_TYPE,
            &current.render_full(),
            &current.fingerprint(),
        )),
    }
}

/// The date-change row for a run that crossed into `today`, against the
/// newest announced fingerprint.
#[must_use]
pub fn date_change_row(announced: &Announced, today: &str) -> Option<CustomMessage> {
    let latest = announced.latest.as_ref()?;
    if !announced.full || latest.date == today {
        return None;
    }
    let mut fingerprint = latest.clone();
    fingerprint.date = today.to_string();
    let text = system_reminder(&format!(
        "{}\n\n{AMBIENT_CONTEXT_NOTE}",
        render_date_change(today)
    ));
    Some(context_row(
        SESSION_CONTEXT_UPDATE_CUSTOM_TYPE,
        &text,
        &fingerprint,
    ))
}

/// One session's context rows and reminders.
pub struct SessionContextRows {
    source: SessionContextSource,
    reminders: Reminders,
    /// Today's date (tests pin it).
    today: Arc<dyn Fn() -> String + Send + Sync>,
    /// The cwd the source reads, when it reads a movable one.
    cwd: Option<Arc<std::sync::Mutex<std::path::PathBuf>>>,
    /// The model facts the source reads, when it reads switchable ones.
    model: Option<Arc<std::sync::Mutex<super::context_source::ModelFacts>>>,
}

fn to_loop(row: CustomMessage) -> Option<AgentMessage> {
    session_message_to_loop(&SessionAgentMessage::Custom(row))
}

impl SessionContextRows {
    #[must_use]
    pub fn new(source: SessionContextSource, reminders: Reminders) -> Self {
        Self {
            source,
            reminders,
            today: Arc::new(crate::prompts::session_context::today_utc),
            cwd: None,
            model: None,
        }
    }

    /// Share the model facts the source reads, so [`Self::set_model`] can
    /// switch them.
    #[must_use]
    pub fn with_model(
        mut self,
        model: Arc<std::sync::Mutex<super::context_source::ModelFacts>>,
    ) -> Self {
        self.model = Some(model);
        self
    }

    /// The session switched models: the next turn's environment update names
    /// it (and its image input), and the token countdown follows its window.
    pub fn set_model(&self, facts: super::context_source::ModelFacts) {
        self.reminders.set_context_window(facts.context_window);
        if let Some(slot) = &self.model {
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = facts;
        }
    }

    /// Share the cwd the source reads, so [`Self::set_cwd`] can move it.
    #[must_use]
    pub fn with_cwd(mut self, cwd: Arc<std::sync::Mutex<std::path::PathBuf>>) -> Self {
        self.cwd = Some(cwd);
        self
    }

    /// The session moved (`/cwd`): the next turn's environment update and
    /// instruction files follow.
    pub fn set_cwd(&self, cwd: &std::path::Path) {
        if let Some(slot) = &self.cwd {
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = cwd.to_path_buf();
        }
    }

    /// Replace the clock's date source (tests).
    #[must_use]
    pub fn with_today(mut self, today: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        self.today = today;
        self
    }

    #[must_use]
    pub fn reminders(&self) -> &Reminders {
        &self.reminders
    }

    /// The full context a fresh conversation's first turn carries (the
    /// `session_context` row's text). Blocking.
    #[must_use]
    pub fn full_context_text(&self) -> String {
        (self.source)().render_full()
    }

    /// The rows an admitted turn carries ahead of its prompt: the context
    /// row (full or delta) and the reminder row, each when due. Blocking.
    #[must_use]
    pub fn turn_start_rows(&self, messages: &[AgentMessage]) -> Vec<CustomMessage> {
        let mut rows = Vec::new();
        let current = (self.source)();
        rows.extend(context_row_for(&current, &announced(messages)));
        rows.extend(
            self.reminders
                .row(messages, ReminderPoint::TurnStart)
                .map(|row| row.custom_message()),
        );
        rows
    }

    /// The rows the run's next model call carries after a tool batch: the
    /// full context when the conversation lost it (a mid-run compaction), a
    /// date change, and the reminder row. Blocking.
    #[must_use]
    pub fn boundary_rows(&self, messages: &[AgentMessage]) -> Vec<CustomMessage> {
        let mut rows = Vec::new();
        let announced = announced(messages);
        if announced.full {
            rows.extend(date_change_row(&announced, &(self.today)()));
        } else {
            rows.extend(context_row_for(&(self.source)(), &announced));
        }
        rows.extend(
            self.reminders
                .row(messages, ReminderPoint::Boundary)
                .map(|row| row.custom_message()),
        );
        rows
    }

    /// [`Self::turn_start_rows`] off the async runtime, in loop form.
    pub async fn turn_start_messages(
        self: &Arc<Self>,
        messages: Vec<AgentMessage>,
    ) -> Vec<AgentMessage> {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || this.turn_start_rows(&messages))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(to_loop)
            .collect()
    }

    /// [`Self::boundary_rows`] off the async runtime, in loop form.
    pub async fn boundary_messages(
        self: &Arc<Self>,
        messages: Vec<AgentMessage>,
    ) -> Vec<AgentMessage> {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || this.boundary_rows(&messages))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(to_loop)
            .collect()
    }
}

/// The run boundary: after a tool batch the run goes on, so the due rows
/// join its next model call through the steering queue. A turn without tool
/// calls ends the run here, and a row would keep it going: nothing is added.
pub(super) async fn after_turn(
    rows: &Arc<SessionContextRows>,
    agent: &pa_agent::agent::Agent,
    turn: &pa_agent::types::ShouldStopAfterTurnContext,
) {
    if turn.tool_results.is_empty() {
        return;
    }
    let batch = rows.boundary_messages(turn.context.messages.clone()).await;
    if !batch.is_empty() {
        agent.steer(batch);
    }
}

#[cfg(test)]
#[path = "context_rows_tests.rs"]
mod tests;
