//! Per-turn reminders (Claude Code's per-turn attachments): short notes the
//! harness adds between tool calls and ahead of a user turn, all in one
//! hidden `<system-reminder>` row.
//!
//! - **Changed files**: a file the model read changed on disk since
//!   (Claude Code's `edited_text_file`), with the changed lines.
//! - **Re-read** (Claude Code's read dedup): a cell re-read a file that had
//!   not changed since its last read in this context.
//! - **Task list**: the bundled `tasks` skill has not been used for 10
//!   assistant turns, and no such reminder came in the last 10 (Claude
//!   Code's `todo_reminder`, `TURNS_SINCE_WRITE` / `TURNS_BETWEEN_REMINDERS`).
//! - **Silent turns**: 5 assistant turns in a row said nothing to the user
//!   (at most 3 such reminders between user messages; Claude Code's
//!   `silent_turn_reminder`).
//! - **Elapsed time** since the session started.
//! - **Tokens left** in the context window (`<total_tokens>`).
//!
//! Each kind has a `reminders.*` setting; the ones Claude Code ships on are
//! on by default (changed files, task list, tokens left), the rest are off.
//! The turn counts read the conversation itself, so a resumed session keeps
//! its cadence.

use std::path::PathBuf;
use std::time::Instant;

use pa_agent::types::{AgentMessage, AssistantContent, Message};
use pa_types::session::CustomMessage;

use super::file_tracker::{FileNotes, FileReadTracker};
use crate::prompts::session_context::system_reminder;

/// The custom type of the hidden reminder row.
pub const SYSTEM_REMINDER_CUSTOM_TYPE: &str = "system_reminder";

/// Claude Code's `Bve`: turns since the last task-list write, and between
/// two task-list reminders.
pub const TODO_TURNS_SINCE_WRITE: usize = 10;
pub const TODO_TURNS_BETWEEN_REMINDERS: usize = 10;
/// Claude Code's silent-turn defaults (`_yn`, `Syn`).
pub const SILENT_TURNS: usize = 5;
pub const SILENT_REMINDERS_PER_STRETCH: usize = 3;
/// How many changed files one row names (the rest wait for the next).
const MAX_CHANGED_FILES_PER_ROW: usize = 8;
/// How many changed-file snippets one row shows in full.
const MAX_SNIPPETS_PER_ROW: usize = 4;
const MAX_REREADS_PER_ROW: usize = 5;

/// Claude Code's `kyn`.
pub const SILENT_TURN_TEXT: &str = "The user hasn't heard from you in a while. As you continue, keep them updated when there's something to tell \u{2014} a finding, a change of plan.";

/// The `reminders` settings, resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderToggles {
    pub changed_files: bool,
    pub read_dedup: bool,
    pub task_list: bool,
    pub silent_turns: bool,
    pub elapsed_time: bool,
    pub tokens_left: bool,
}

impl Default for ReminderToggles {
    /// Claude Code's defaults: on for changed files, the task list and the
    /// token countdown; the model-gated nudges (silent turns, elapsed time)
    /// off. The re-read note is off too: a kernel read does not show the
    /// model the file the way Claude Code's Read tool does, so "already in
    /// your context" is not something the host can know.
    fn default() -> Self {
        Self {
            changed_files: true,
            read_dedup: false,
            task_list: true,
            silent_turns: false,
            elapsed_time: false,
            tokens_left: true,
        }
    }
}

/// Which reminder a row carries (its details' `kinds`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderKind {
    ChangedFiles,
    Reread,
    TaskList,
    SilentTurns,
    ElapsedTime,
    TokensLeft,
}

impl ReminderKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChangedFiles => "changed_files",
            Self::Reread => "reread",
            Self::TaskList => "task_list",
            Self::SilentTurns => "silent_turns",
            Self::ElapsedTime => "elapsed_time",
            Self::TokensLeft => "tokens_left",
        }
    }
}

/// Where a reminder row is being added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReminderPoint {
    /// Ahead of a user turn's prompt.
    TurnStart,
    /// Between a tool batch and the next model call of a run.
    Boundary,
}

/// The session's reminder state and inputs.
pub struct Reminders {
    toggles: ReminderToggles,
    started: Instant,
    files: std::sync::Arc<FileReadTracker>,
    /// The `tasks` skill's store (`<session dir>/tasks.json`); `None` when
    /// the skill is not available to the model or the session keeps no
    /// directory (then only the nag, without the list).
    tasks_file: Option<PathBuf>,
    /// Whether the `tasks` skill is available to the model at all.
    has_tasks: bool,
    /// The context window the countdown counts down from (0: unknown, off).
    context_window: u64,
}

/// One rendered reminder row and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReminderRow {
    pub text: String,
    pub kinds: Vec<ReminderKind>,
}

impl ReminderRow {
    /// The hidden conversation row.
    #[must_use]
    pub fn custom_message(&self) -> CustomMessage {
        let mut row = super::plan_mode::hidden_row(SYSTEM_REMINDER_CUSTOM_TYPE, &self.text);
        row.details = Some(serde_json::json!({
            "kinds": self.kinds.iter().map(|kind| kind.as_str()).collect::<Vec<_>>(),
        }));
        row
    }
}

fn custom_type(message: &AgentMessage) -> Option<&str> {
    match message {
        AgentMessage::Custom(custom) => custom
            .payload
            .get("customType")
            .and_then(serde_json::Value::as_str),
        AgentMessage::Standard(_) => None,
    }
}

fn reminder_kinds(message: &AgentMessage) -> Vec<String> {
    if custom_type(message) != Some(SYSTEM_REMINDER_CUSTOM_TYPE) {
        return Vec::new();
    }
    let AgentMessage::Custom(custom) = message else {
        return Vec::new();
    };
    custom
        .payload
        .get("details")
        .and_then(|details| details.get("kinds"))
        .and_then(serde_json::Value::as_array)
        .map(|kinds| {
            kinds
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn carries(message: &AgentMessage, kind: ReminderKind) -> bool {
    reminder_kinds(message)
        .iter()
        .any(|carried| carried == kind.as_str())
}

/// Whether an assistant turn wrote the task list (a `tasks.create`/
/// `tasks.update` call in a Python cell).
fn writes_task_list(message: &pa_agent::types::AssistantMessage) -> bool {
    message.content.iter().any(|block| match block {
        AssistantContent::ToolCall(call) => {
            let code = call
                .arguments
                .get("code")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            ["tasks.create(", "tasks.update("]
                .iter()
                .any(|needle| code.contains(needle))
        }
        AssistantContent::Text(_) | AssistantContent::Thinking(_) => false,
    })
}

/// Claude Code's `$Lr`: assistant turns since the last task-list write and
/// since the last task-list reminder (newest first).
#[must_use]
pub fn task_list_turns(messages: &[AgentMessage]) -> (usize, usize) {
    let mut since_write = None;
    let mut since_reminder = None;
    let mut assistants = 0;
    for message in messages.iter().rev() {
        match message {
            AgentMessage::Standard(Message::Assistant(assistant)) => {
                if since_write.is_none() && writes_task_list(assistant) {
                    since_write = Some(assistants);
                }
                assistants += 1;
            }
            other if since_reminder.is_none() && carries(other, ReminderKind::TaskList) => {
                since_reminder = Some(assistants);
            }
            _ => {}
        }
        if since_write.is_some() && since_reminder.is_some() {
            break;
        }
    }
    (
        since_write.unwrap_or(assistants),
        since_reminder.unwrap_or(assistants),
    )
}

/// Whether an assistant turn told the user something (non-empty reply text).
fn speaks(message: &pa_agent::types::AssistantMessage) -> bool {
    message.content.iter().any(|block| match block {
        AssistantContent::Text(text) => !text.text.trim().is_empty(),
        AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => false,
    })
}

/// Claude Code's `VDr`, over this conversation: silent assistant turns since
/// the last silent-turn reminder, and how many such reminders this stretch
/// (since the user's last message) already holds.
#[must_use]
pub fn silent_turns(messages: &[AgentMessage]) -> (usize, usize) {
    let mut since_reminder = 0;
    let mut reminder_seen = false;
    let mut reminders = 0;
    for message in messages.iter().rev() {
        match message {
            AgentMessage::Standard(Message::Assistant(assistant)) => {
                if speaks(assistant) {
                    break;
                }
                if !reminder_seen {
                    since_reminder += 1;
                }
            }
            AgentMessage::Standard(Message::User(_)) => break,
            other if carries(other, ReminderKind::SilentTurns) => {
                reminders += 1;
                reminder_seen = true;
            }
            _ => {}
        }
    }
    (since_reminder, reminders)
}

/// Claude Code's `bDr`.
#[must_use]
pub fn elapsed_text(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    format!(
        "Elapsed time so far: {}m {:02}s",
        seconds / 60,
        seconds % 60
    )
}

/// Claude Code's `zqt` (countdown form).
#[must_use]
pub fn tokens_left_text(left: u64) -> String {
    format!("<total_tokens>{left} tokens left</total_tokens>")
}

/// The context the newest assistant reply reported, as `calculate_context_tokens` counts it.
fn context_tokens(messages: &[AgentMessage]) -> Option<u64> {
    messages.iter().rev().find_map(|message| match message {
        AgentMessage::Standard(Message::Assistant(assistant)) => {
            let usage = serde_json::to_value(&assistant.usage).ok()?;
            let tokens = pa_types::usage::calculate_context_tokens(&usage);
            (tokens > 0).then_some(tokens)
        }
        _ => None,
    })
}

/// Claude Code's `edited_text_file` note for one file.
#[must_use]
pub fn changed_file_text(path: &std::path::Path, snippet: Option<&str>) -> String {
    let head = format!(
        "Note: {} changed on disk since you last read it. That's usually deliberate, so take it as the current state rather than reverting it; if the change looks wrong, say so rather than undoing it yourself \u{2014} otherwise no need to call it out.",
        path.display()
    );
    match snippet {
        Some(snippet) => {
            format!("{head} Here are the relevant changes (shown with line numbers):\n{snippet}")
        }
        None => format!(
            "{head} The changes are not shown here; read the file again if you need the current content."
        ),
    }
}

/// The re-read note (Claude Code's `file_unchanged` text, for reads the
/// host can only observe after the fact).
#[must_use]
pub fn reread_text(paths: &[PathBuf]) -> String {
    let list = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Note: re-read without a change since your last read: {list}. Refer to the earlier output (or the variable you read it into) instead of reading it again."
    )
}

fn task_list_text(tasks_file: Option<&std::path::Path>) -> String {
    let mut text = "The task list hasn't been used recently. If you're working on tasks that would benefit from tracking progress, consider using `await tasks.create(...)` to add new tasks and `await tasks.update(id, status=...)` to update their status (in_progress when starting, completed when done). Also consider cleaning up the task list if it has become stale. Only use it if relevant to the current work. This is just a gentle reminder - ignore if not applicable.".to_string();
    let tasks = tasks_file
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|store| {
            store
                .get("tasks")
                .and_then(serde_json::Value::as_array)
                .cloned()
        })
        .unwrap_or_default();
    let rows: Vec<String> = tasks
        .iter()
        .filter_map(|task| {
            let status = task.get("status")?.as_str()?;
            if status == "deleted" {
                return None;
            }
            Some(format!(
                "#{}. [{status}] {}",
                task.get("id")?.as_str()?,
                task.get("subject")?.as_str()?
            ))
        })
        .collect();
    if !rows.is_empty() {
        text.push_str("\n\nHere are the existing tasks:\n\n");
        text.push_str(&rows.join("\n"));
    }
    text
}

impl Reminders {
    #[must_use]
    pub fn new(
        toggles: ReminderToggles,
        files: std::sync::Arc<FileReadTracker>,
        has_tasks: bool,
        tasks_file: Option<PathBuf>,
        context_window: u64,
    ) -> Self {
        Self {
            toggles,
            started: Instant::now(),
            files,
            tasks_file,
            has_tasks,
            context_window,
        }
    }

    #[must_use]
    pub fn toggles(&self) -> ReminderToggles {
        self.toggles
    }

    /// The tracker the kernel's file accesses feed.
    #[must_use]
    pub fn files(&self) -> &std::sync::Arc<FileReadTracker> {
        &self.files
    }

    /// The row to add at `point` for a conversation ending in `messages`,
    /// `None` when nothing is due. Blocking file I/O (changed files, the task
    /// store): run it off the async runtime.
    #[must_use]
    pub fn row(&self, messages: &[AgentMessage], point: ReminderPoint) -> Option<ReminderRow> {
        let mut parts: Vec<String> = Vec::new();
        let mut kinds = Vec::new();
        let notes = if self.toggles.changed_files || self.toggles.read_dedup {
            self.files.boundary()
        } else {
            FileNotes::default()
        };
        if self.toggles.changed_files && !notes.changed.is_empty() {
            for (index, changed) in notes
                .changed
                .iter()
                .take(MAX_CHANGED_FILES_PER_ROW)
                .enumerate()
            {
                let snippet = (index < MAX_SNIPPETS_PER_ROW)
                    .then_some(changed.snippet.as_deref())
                    .flatten();
                parts.push(changed_file_text(&changed.path, snippet));
            }
            kinds.push(ReminderKind::ChangedFiles);
        }
        if self.toggles.read_dedup && !notes.rereads.is_empty() {
            let shown: Vec<PathBuf> = notes
                .rereads
                .iter()
                .take(MAX_REREADS_PER_ROW)
                .cloned()
                .collect();
            parts.push(reread_text(&shown));
            kinds.push(ReminderKind::Reread);
        }
        if self.toggles.task_list && self.has_tasks {
            let (since_write, since_reminder) = task_list_turns(messages);
            if since_write >= TODO_TURNS_SINCE_WRITE
                && since_reminder >= TODO_TURNS_BETWEEN_REMINDERS
            {
                parts.push(task_list_text(self.tasks_file.as_deref()));
                kinds.push(ReminderKind::TaskList);
            }
        }
        if point == ReminderPoint::Boundary {
            if self.toggles.silent_turns {
                let (since_reminder, reminders) = silent_turns(messages);
                if since_reminder >= SILENT_TURNS && reminders < SILENT_REMINDERS_PER_STRETCH {
                    parts.push(SILENT_TURN_TEXT.to_string());
                    kinds.push(ReminderKind::SilentTurns);
                }
            }
            if self.toggles.elapsed_time {
                parts.push(elapsed_text(self.started.elapsed()));
                kinds.push(ReminderKind::ElapsedTime);
            }
        }
        if self.toggles.tokens_left && self.context_window > 0 {
            if let Some(used) = context_tokens(messages) {
                parts.push(tokens_left_text(self.context_window.saturating_sub(used)));
                kinds.push(ReminderKind::TokensLeft);
            }
        }
        if parts.is_empty() {
            return None;
        }
        Some(ReminderRow {
            text: system_reminder(&parts.join("\n\n")),
            kinds,
        })
    }
}

#[cfg(test)]
#[path = "reminders_tests.rs"]
mod tests;
