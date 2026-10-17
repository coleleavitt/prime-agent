//! Brief mode: the model's replies reach the user through
//! `rlm.send_user_message`, and plain assistant text is collapsed.
//!
//! Claude Code 2.1.296's brief mode (`CLAUDE_CODE_BRIEF` or
//! `tengu_kairos_brief`, unreleased; `gek2r58t`, `mvz09dzd`, `/brief` in
//! `2czmwb85`) gives the model a `SendUserMessage` tool, "your primary visible
//! output channel": its message renders prominently, while text outside it
//! is visible only in the detail view. The system prompt's "Talking to the
//! user" section says so; `/brief` toggles brief-only mode with a system
//! reminder; and a Stop hook re-prompts once when a turn ends without the
//! call (`"You ended the turn without calling SendUserMessage."` plus the
//! enforce text).
//!
//! Here:
//!
//! - `experimental.briefMode` (or `PRIME_AGENT_BRIEF`) starts root sessions
//!   in brief mode, with [`BRIEF_SECTION`] in the system prompt; `/brief
//!   [on|off|status]` switches it per session. Each change records a durable
//!   `brief_mode_change` row (a resumed session restores the newest one) and
//!   a hidden reminder row (Claude Code's toggle reminder; turning on carries
//!   the section, so a session started without it learns the channel);
//! - `rlm.send_user_message(message, attachments=None, *, status="normal")`
//!   (the `user_message.send` host request, refused outside brief mode)
//!   records a displayed `user_message` row the TUI renders prominently;
//!   the model never receives the row back (its own cell holds the text);
//! - at a natural stop of a root run with no `rlm.send_user_message` since
//!   the newest user prompt, one hidden enforcement row continues the run;
//! - the `brief` feature status (`{"enabled": bool}`) tells the TUI to
//!   collapse the assistant text of turns that sent a user message.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use pa_agent::types::{AgentMessage, Message, ShouldStopAfterTurnContext, StopReason};
use pa_types::session::{CustomMessage, FileEntry};
use serde_json::{Value, json};

use crate::kernel::shared::{HostRequestHandlers, host_handler};

/// The host request behind `rlm.send_user_message`.
pub const SEND_USER_MESSAGE_REQUEST: &str = "user_message.send";
/// The displayed row one `rlm.send_user_message` records.
pub const USER_MESSAGE_CUSTOM_TYPE: &str = "user_message";
/// The durable, displayed row recording a brief-mode change
/// (`details.enabled`).
pub const BRIEF_MODE_CHANGE_CUSTOM_TYPE: &str = "brief_mode_change";
/// The hidden row telling the model the mode changed.
pub const BRIEF_MODE_REMINDER_CUSTOM_TYPE: &str = "brief_mode_reminder";
/// The hidden row a turn that ended without a user message continues with.
pub const BRIEF_ENFORCEMENT_CUSTOM_TYPE: &str = "brief_enforcement";
/// The feature status the TUI keys its collapsing on.
pub const BRIEF_FEATURE: &str = "brief";

/// Claude Code's "Talking to the user" system-prompt section (`O2s`), with
/// the kernel function in place of the `SendUserMessage` tool and its
/// description (`PSo`).
pub const BRIEF_SECTION: &str = "## Talking to the user

`rlm.send_user_message(message: str, attachments: list[str] | None = None, *, status: str = \"normal\") -> dict` sends a message the user will read. Text outside it is visible in the detail view, but most won't open it \u{2014} the answer lives here. `message` supports markdown. `attachments` are file paths (absolute or relative to the working directory) for files the user should see alongside the message \u{2014} images, diffs, logs. `status` labels intent: \"normal\" when replying to what they just asked; \"proactive\" when you're initiating \u{2014} a background task finished, a blocker surfaced during background work, you need input on something they haven't asked about. Set it honestly.

`rlm.send_user_message` is where your replies go. Text outside it is visible if the user expands the detail view, but most won't \u{2014} assume unread. Anything you want them to actually see goes through `rlm.send_user_message`. The failure mode: the real answer lives in plain text while `rlm.send_user_message` just says \"done!\" \u{2014} they see \"done!\" and miss everything.

So: every time the user says something, the reply they actually read comes through `rlm.send_user_message`. Even for \"hi\". Even for \"thanks\".

If you can answer right away, send the answer. If you need to go look \u{2014} run a command, read files, check something \u{2014} ack first in one line (\"On it \u{2014} checking the test output\"), then work, then send the result. Without the ack they're staring at a spinner.

For longer work: ack \u{2192} work \u{2192} result. Between those, send a checkpoint when something useful happened \u{2014} a decision you made, a surprise you hit, a phase boundary. Skip the filler (\"running tests...\") \u{2014} a checkpoint earns its place by carrying information.

Keep messages tight \u{2014} the decision, the file:line, the PR number. Second person always (\"your config\"), never third.";

/// Claude Code's enforce sentinel (`Xen`), in the kernel's vocabulary.
pub const BRIEF_ENFORCE_SENTINEL: &str =
    "You ended the turn without calling rlm.send_user_message.";

/// Claude Code's default enforce text (`gek2r58t:r`).
const BRIEF_ENFORCE_TEXT: &str = "In brief mode, plain assistant text is hidden from the user \u{2014} only rlm.send_user_message reaches them. Call it now with your substantive reply for this turn. Do not mention this reminder; the message should read as if you wrote it unprompted, addressing only what the user actually asked. If you genuinely have nothing useful to tell the user, you may end the turn without calling it.";

/// Claude Code's `/brief` toggle reminders.
const ENABLED_TEXT: &str = "Brief mode is now enabled. Use rlm.send_user_message for all user-facing output \u{2014} plain text outside it is hidden from the user's view.";
const DISABLED_TEXT: &str = "Brief mode is now disabled. rlm.send_user_message is no longer available \u{2014} reply with plain text.";

/// The refusal outside brief mode.
const NOT_IN_BRIEF_MODE: &str = "rlm.send_user_message is only available in brief mode; the user turns it on with /brief. Reply with plain text.";

/// The most attachments one message carries.
const MAX_ATTACHMENTS: usize = 20;

fn now_millis() -> i64 {
    pa_agent::now_ms()
}

/// One session's brief mode.
#[derive(Debug, Default)]
pub struct BriefState {
    on: AtomicBool,
    /// When `rlm.send_user_message` last delivered (ms since the epoch).
    last_sent_ms: AtomicI64,
}

impl BriefState {
    #[must_use]
    pub fn new(on: bool) -> Self {
        Self {
            on: AtomicBool::new(on),
            last_sent_ms: AtomicI64::new(0),
        }
    }

    #[must_use]
    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    /// Switch the mode; returns whether it changed.
    pub fn set(&self, on: bool) -> bool {
        self.on.swap(on, Ordering::SeqCst) != on
    }

    fn note_sent(&self) {
        self.last_sent_ms.fetch_max(now_millis(), Ordering::SeqCst);
    }

    /// Whether a user message went out at or after `since_ms`.
    #[must_use]
    pub fn sent_since(&self, since_ms: i64) -> bool {
        let sent = self.last_sent_ms.load(Ordering::SeqCst);
        sent > 0 && sent >= since_ms
    }
}

/// The durable, displayed row recording a mode change.
#[must_use]
pub fn brief_mode_change_row(enabled: bool) -> CustomMessage {
    let text = if enabled {
        "Brief-only mode enabled: replies arrive as messages; other assistant text is collapsed (/brief off to stop)."
    } else {
        "Brief-only mode disabled."
    };
    CustomMessage {
        custom_type: BRIEF_MODE_CHANGE_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(text.to_string()),
        display: true,
        details: Some(json!({ "enabled": enabled })),
        timestamp: u64::try_from(now_millis()).unwrap_or_default(),
        rest: serde_json::Map::default(),
    }
}

/// The hidden reminder a mode change carries (turning on includes the
/// section, so a session whose prompt lacks it learns the channel).
#[must_use]
pub fn brief_mode_reminder_row(enabled: bool) -> CustomMessage {
    let text = if enabled {
        format!("{ENABLED_TEXT}\n\n{BRIEF_SECTION}")
    } else {
        DISABLED_TEXT.to_string()
    };
    super::plan_mode::hidden_row(
        BRIEF_MODE_REMINDER_CUSTOM_TYPE,
        &crate::prompts::session_context::system_reminder(&text),
    )
}

fn change_enabled(custom_type: &str, details: Option<&Value>) -> Option<bool> {
    if custom_type != BRIEF_MODE_CHANGE_CUSTOM_TYPE {
        return None;
    }
    details?.get("enabled")?.as_bool()
}

/// The newest brief-mode change along a branch's entries (oldest first).
#[must_use]
pub fn brief_mode_in_entries<'a>(
    entries: impl DoubleEndedIterator<Item = &'a FileEntry>,
) -> Option<bool> {
    entries.rev().find_map(|entry| match entry {
        FileEntry::CustomMessage { payload, .. } => {
            change_enabled(&payload.custom_type, payload.details.as_ref())
        }
        FileEntry::Message {
            message: pa_types::session::AgentMessage::Custom(message),
            ..
        } => change_enabled(&message.custom_type, message.details.as_ref()),
        _ => None,
    })
}

/// One `/brief` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BriefCommand {
    Toggle,
    On,
    Off,
    Status,
}

/// Parse `/brief [on|off|status]`; bare `/brief` toggles (Claude Code's).
///
/// # Errors
///
/// Returns the usage text for any other argument.
pub fn parse_brief_command(args: &str) -> Result<BriefCommand, String> {
    match args.trim() {
        "" => Ok(BriefCommand::Toggle),
        "on" => Ok(BriefCommand::On),
        "off" => Ok(BriefCommand::Off),
        "status" => Ok(BriefCommand::Status),
        _ => Err("Usage: /brief [on|off|status]".to_string()),
    }
}

/// The `brief` feature status for the TUI.
#[must_use]
pub fn brief_status(enabled: bool) -> crate::features::FeatureStatus {
    crate::features::FeatureStatus {
        feature: BRIEF_FEATURE.to_string(),
        line: enabled.then(|| "brief mode".to_string()),
        status: json!({ "enabled": enabled }),
    }
}

/// One validated `user_message.send` request.
#[derive(Debug, Clone, PartialEq)]
pub struct UserMessageRequest {
    pub message: String,
    pub attachments: Vec<String>,
    pub proactive: bool,
}

/// Validate a `user_message.send` payload.
///
/// # Errors
///
/// A missing or empty message, a malformed attachment list, or an unknown
/// status.
pub fn parse_user_message(data: &Value) -> anyhow::Result<UserMessageRequest> {
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("rlm.send_user_message message must be a non-empty string")
        })?;
    let attachments = match data.get("attachments") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "rlm.send_user_message attachments must be non-empty path strings"
                        )
                    })
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
        Some(_) => anyhow::bail!("rlm.send_user_message attachments must be a list of paths"),
    };
    if attachments.len() > MAX_ATTACHMENTS {
        anyhow::bail!("rlm.send_user_message takes at most {MAX_ATTACHMENTS} attachments");
    }
    let proactive = match data.get("status").and_then(Value::as_str) {
        None | Some("normal") => false,
        Some("proactive") => true,
        Some(other) => anyhow::bail!(
            "rlm.send_user_message status must be \"normal\" or \"proactive\", got {other:?}"
        ),
    };
    Ok(UserMessageRequest {
        message: message.to_string(),
        attachments,
        proactive,
    })
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
            )
        })
}

/// Resolve the attachments against `cwd`: each must be a readable file.
fn resolve_attachments(cwd: &Path, attachments: &[String]) -> anyhow::Result<Vec<Value>> {
    attachments
        .iter()
        .map(|raw| {
            let path = {
                let given = PathBuf::from(raw);
                if given.is_absolute() {
                    given
                } else {
                    cwd.join(given)
                }
            };
            let meta = std::fs::metadata(&path).map_err(|error| {
                anyhow::anyhow!("rlm.send_user_message attachment {raw}: {error}")
            })?;
            if !meta.is_file() {
                anyhow::bail!("rlm.send_user_message attachment {raw} is not a file");
            }
            Ok(json!({
                "path": path.display().to_string(),
                "size": meta.len(),
                "isImage": is_image_path(&path),
            }))
        })
        .collect()
}

/// The displayed row for one message.
#[must_use]
pub fn user_message_row(request: &UserMessageRequest, attachments: &[Value]) -> CustomMessage {
    let sent_at = now_millis();
    CustomMessage {
        custom_type: USER_MESSAGE_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(request.message.clone()),
        display: true,
        details: Some(json!({
            "status": if request.proactive { "proactive" } else { "normal" },
            "attachments": attachments,
            "sentAt": sent_at,
        })),
        timestamp: u64::try_from(sent_at).unwrap_or_default(),
        rest: serde_json::Map::default(),
    }
}

/// What the `user_message.send` handler needs.
pub struct UserMessageContext {
    pub cwd: PathBuf,
    pub state: Arc<BriefState>,
    /// The row seam `artifact.present` uses (persist + broadcast where the
    /// host installed a sink; the engine's own session otherwise).
    pub rows: Arc<super::presented_artifact::PresentedArtifacts>,
    pub session: Arc<tokio::sync::Mutex<crate::session::manager::SessionManager>>,
}

/// Register `user_message.send`.
pub fn register_host_handlers(handlers: &mut HostRequestHandlers, context: UserMessageContext) {
    let context = Arc::new(context);
    // The literal (`SEND_USER_MESSAGE_REQUEST`) keeps the prompt guard's
    // registration scan exact.
    handlers.register(
        "user_message.send",
        host_handler(move |payload| {
            let context = Arc::clone(&context);
            Box::pin(async move {
                if !context.state.is_on() {
                    anyhow::bail!(NOT_IN_BRIEF_MODE);
                }
                let request = parse_user_message(&payload.data)?;
                let attachments = resolve_attachments(&context.cwd, &request.attachments)?;
                let count = attachments.len();
                let row = user_message_row(&request, &attachments);
                if let Some(sink) = context.rows.sink() {
                    sink(row)?;
                } else {
                    let mut session = context.session.lock().await;
                    session.append_custom_message(
                        &row.custom_type,
                        row.content,
                        row.display,
                        row.details,
                    )?;
                }
                context.state.note_sent();
                Ok(json!({ "delivered": true, "attachments": count }))
            })
        }),
    );
}

/// The newest user prompt's timestamp in a run's context.
fn last_prompt_ms(messages: &[AgentMessage]) -> i64 {
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            AgentMessage::Standard(Message::User(user)) => Some(user.timestamp),
            _ => None,
        })
        .unwrap_or(0)
}

fn is_enforcement_row(message: &AgentMessage) -> bool {
    matches!(message, AgentMessage::Custom(row)
        if row.payload.get("customType").and_then(Value::as_str)
            == Some(BRIEF_ENFORCEMENT_CUSTOM_TYPE))
}

/// The enforcement text a turn that ended without a user message
/// continues with, or `None` when the turn may stop: brief mode is off, the
/// turn is not a natural stop, a message went out since the newest prompt,
/// or this run was already reminded once.
#[must_use]
pub fn enforcement_text(state: &BriefState, turn: &ShouldStopAfterTurnContext) -> Option<String> {
    if !state.is_on()
        || !turn.tool_results.is_empty()
        || turn.message.stop_reason != StopReason::Stop
    {
        return None;
    }
    if state.sent_since(last_prompt_ms(&turn.context.messages)) {
        return None;
    }
    if turn.new_messages.iter().any(is_enforcement_row) {
        return None;
    }
    Some(crate::prompts::session_context::system_reminder(&format!(
        "{BRIEF_ENFORCE_SENTINEL} {BRIEF_ENFORCE_TEXT}"
    )))
}

/// Continue the stopping run with the enforcement row ([`enforcement_text`])
/// unless queued input continues it anyway.
pub(super) fn steer_enforcement(slot: &super::steering_fold::FoldAdmissionSlot, text: &str) {
    let Some(agent) = slot.get().and_then(|admission| admission.agent.upgrade()) else {
        return;
    };
    if agent.has_queued_messages() {
        return;
    }
    super::feature_boundary::steer_hidden_row(&agent, BRIEF_ENFORCEMENT_CUSTOM_TYPE, text);
}

#[cfg(test)]
#[path = "brief_mode_tests.rs"]
mod tests;
