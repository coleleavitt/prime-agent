//! Ultracode: Claude Code's opt-in to multi-agent orchestration.
//!
//! Two ways in, both delivered as hidden reminder rows riding ahead of the
//! user's prompt (never a system-prompt change, so the cached prefix stays
//! stable):
//!
//! - **The keyword.** A prompt that contains the whole word `ultracode`
//!   (Claude Code's exact detection rules: [`keyword_matches`]) opts that one
//!   turn into orchestration: the model is told to fulfil it with
//!   `rlm.workflow`. The `workflowKeywordTriggerEnabled` setting (default
//!   on, like Claude Code) turns detection off.
//! - **The standing mode.** `/ultracode on` (or `/effort ultracode`) keeps
//!   the opt-in for every turn: a full reminder on the first turn after it
//!   turns on, a sparse one every [`TURNS_BETWEEN_REMINDERS`] user turns,
//!   and a one-shot notice when it turns off. The mode is session state: a
//!   durable `ultracode_change` row records each change and a resumed
//!   session restores the newest one (the `ultracode` setting is the default
//!   for a session that never changed it; off unless set).
//!
//! The keyword reminder carries the `workflowSizeGuideline` setting's agent
//! count (`small` 5, `medium` 10 by default, `large` 50, `unrestricted`).
//!
//! The reminder wrapper here is local ([`reminder_row`]); the shared
//! system-reminder rows the context work introduces replace it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pa_types::session::{AgentMessage, CustomMessage, FileEntry};
use pa_types::sync::MutexExt;

/// The per-turn row a prompt with the keyword carries (not displayed).
pub const ULTRACODE_KEYWORD_CUSTOM_TYPE: &str = "ultracode_keyword";
/// The standing mode's entry and periodic reminders (not displayed).
pub const ULTRACODE_REMINDER_CUSTOM_TYPE: &str = "ultracode_reminder";
/// The one-shot row for the first turn after the mode turns off (not displayed).
pub const ULTRACODE_EXIT_CUSTOM_TYPE: &str = "ultracode_exit";
/// The durable, displayed row recording a mode change (`details.enabled`).
pub const ULTRACODE_CHANGE_CUSTOM_TYPE: &str = "ultracode_change";

/// User turns between two sparse "still on" reminders (Claude Code's
/// `TURNS_BETWEEN_MAINTENANCE`).
pub const TURNS_BETWEEN_REMINDERS: u32 = 10;

const SETTINGS_FILE_NAME: &str = "settings.json";

const KEYWORD_TEXT: &str = "The user included the keyword \"ultracode\", opting this turn into multi-agent orchestration \u{2014} use rlm.workflow to fulfill the request (load the workflow-authoring skill before writing the script).";
const ENTER_TEXT: &str = "Ultracode is on: optimize for the most exhaustive, correct answer \u{2014} not the fastest or cheapest. Use rlm.workflow on every substantive task; token cost is not a constraint. See the **Ultracode** section and quality patterns in the workflow-authoring skill. Solo only on conversational/trivial turns.";
const STILL_ON_TEXT: &str = "Ultracode is still on \u{2014} use rlm.workflow; see the Ultracode section of the workflow-authoring skill.";
const EXIT_TEXT: &str =
    "Ultracode is off \u{2014} the rlm.workflow standard opt-in rule applies again.";

/// One whole-word occurrence of a keyword: char offsets into the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordMatch {
    pub word: String,
    pub start: usize,
    pub end: usize,
}

/// The closing character of an opener Claude Code treats as quoting.
fn closer(opener: char) -> Option<char> {
    match opener {
        '`' => Some('`'),
        '"' => Some('"'),
        '<' => Some('>'),
        '{' => Some('}'),
        '[' => Some(']'),
        '(' => Some(')'),
        '\'' => Some('\''),
        _ => None,
    }
}

/// `/[\p{L}\p{N}_]/u`: a letter, a number or `_`.
fn is_word_char(ch: Option<char>) -> bool {
    ch.is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
}

/// JS `\b` without the `u` flag: ASCII word characters only.
fn is_ascii_word(ch: Option<char>) -> bool {
    ch.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// The spans Claude Code skips: text inside quotes, backticks, brackets,
/// parentheses, braces and `<tags>`. An apostrophe inside a word
/// (`don't`) neither opens nor closes a quote; a nested `[` restarts the
/// bracket span; an unclosed opener protects nothing.
fn quoted_spans(chars: &[char]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut open: Option<char> = None;
    let mut start = 0;
    for (index, &ch) in chars.iter().enumerate() {
        if let Some(opener) = open {
            if opener == '[' && ch == '[' {
                start = index;
                continue;
            }
            if Some(ch) != closer(opener) {
                continue;
            }
            if opener == '\'' && is_word_char(chars.get(index + 1).copied()) {
                continue;
            }
            spans.push((start, index + 1));
            open = None;
        } else {
            let previous = index.checked_sub(1).and_then(|at| chars.get(at).copied());
            let opens_tag = ch == '<'
                && chars
                    .get(index + 1)
                    .is_some_and(|next| next.is_ascii_alphabetic() || *next == '/');
            let opens_quote = ch == '\'' && !is_word_char(previous);
            let opens_other = ch != '<' && ch != '\'' && closer(ch).is_some();
            if opens_tag || opens_quote || opens_other {
                open = Some(ch);
                start = index;
            }
        }
    }
    spans
}

/// Claude Code's keyword detector (`Qp`): case-insensitive whole-word
/// occurrences of `keyword` (ASCII) in `text`, skipping a prompt that
/// starts with `/` (a command), occurrences inside quoting
/// ([`quoted_spans`]), occurrences right after `/`, `\` or `-`, right
/// before `/`, `\`, `-` or `?`, and before a `.` that continues a word
/// (`ultracode.md`).
#[must_use]
pub fn keyword_matches(text: &str, keyword: &str) -> Vec<KeywordMatch> {
    if text.starts_with('/') || keyword.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = keyword.chars().collect();
    let length = pattern.len();
    if chars.len() < length {
        return Vec::new();
    }
    let spans = quoted_spans(&chars);
    let mut matches = Vec::new();
    let mut index = 0;
    while index + length <= chars.len() {
        let candidate = &chars[index..index + length];
        let equal = candidate
            .iter()
            .zip(&pattern)
            .all(|(left, right)| left.eq_ignore_ascii_case(right));
        let before = index.checked_sub(1).and_then(|at| chars.get(at).copied());
        let after = chars.get(index + length).copied();
        if !equal || is_ascii_word(before) || is_ascii_word(after) {
            index += 1;
            continue;
        }
        let end = index + length;
        let quoted = spans
            .iter()
            .any(|(start, stop)| index >= *start && index < *stop);
        let blocked_before = matches!(before, Some('/' | '\\' | '-'));
        let blocked_after = matches!(after, Some('/' | '\\' | '-' | '?'))
            || (after == Some('.') && is_word_char(chars.get(end + 1).copied()));
        if !quoted && !blocked_before && !blocked_after {
            matches.push(KeywordMatch {
                word: candidate.iter().collect(),
                start: index,
                end,
            });
        }
        index = end;
    }
    matches
}

/// Whether `text` opts its turn into orchestration with `ultracode`.
#[must_use]
pub fn has_ultracode_keyword(text: &str) -> bool {
    !keyword_matches(text, "ultracode").is_empty()
}

/// The `workflowSizeGuideline` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowSizeGuideline {
    Small,
    Medium,
    Large,
    Unrestricted,
}

impl WorkflowSizeGuideline {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "small" => Some(Self::Small),
            "medium" => Some(Self::Medium),
            "large" => Some(Self::Large),
            "unrestricted" => Some(Self::Unrestricted),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Unrestricted => "unrestricted",
        }
    }

    /// The agent count a workflow should stay under; `None` when unrestricted.
    #[must_use]
    pub fn agents(self) -> Option<u32> {
        match self {
            Self::Small => Some(5),
            Self::Medium => Some(10),
            Self::Large => Some(50),
            Self::Unrestricted => None,
        }
    }
}

/// The settings ultracode reads from the agent dir's `settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UltracodeSettings {
    /// `workflowKeywordTriggerEnabled` (default true).
    pub keyword_trigger: bool,
    /// `workflowSizeGuideline`, `None` when unset (the default `medium`).
    pub size_guideline: Option<WorkflowSizeGuideline>,
    /// `ultracode`: the standing mode for a session that never changed it.
    pub standing_default: bool,
}

impl Default for UltracodeSettings {
    fn default() -> Self {
        Self {
            keyword_trigger: true,
            size_guideline: None,
            standing_default: false,
        }
    }
}

impl UltracodeSettings {
    /// Read the settings; a missing or unreadable file is the defaults.
    #[must_use]
    pub fn load(agent_dir: Option<&Path>) -> Self {
        let Some(agent_dir) = agent_dir else {
            return Self::default();
        };
        let Ok(raw) = std::fs::read_to_string(agent_dir.join(SETTINGS_FILE_NAME)) else {
            return Self::default();
        };
        let Ok(document) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return Self::default();
        };
        Self {
            keyword_trigger: document
                .get("workflowKeywordTriggerEnabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            size_guideline: document
                .get("workflowSizeGuideline")
                .and_then(serde_json::Value::as_str)
                .and_then(WorkflowSizeGuideline::parse),
            standing_default: document
                .get("ultracode")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        }
    }

    /// The size-guideline sentence (Claude Code's wording); `None` when unrestricted.
    #[must_use]
    pub fn guideline_text(&self) -> Option<String> {
        let guideline = self.size_guideline.unwrap_or(WorkflowSizeGuideline::Medium);
        let agents = guideline.agents()?;
        let which = if self.size_guideline.is_some() {
            "the"
        } else {
            "the default"
        };
        Some(format!(
            "This session has {which} workflow size guideline: {} \u{2014} keep workflows under {agents} agents. This is a guideline, not a hard limit \u{2014} follow it unless the user's prompt calls for a different scale. The user can raise or remove it with the workflowSizeGuideline setting.",
            guideline.name()
        ))
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// A hidden system-reminder row: the seam the shared reminder wrapper replaces.
fn reminder_row(custom_type: &str, text: &str) -> CustomMessage {
    CustomMessage {
        custom_type: custom_type.to_string(),
        content: pa_types::ai::UserContent::Text(format!(
            "<system-reminder>\n{text}\n</system-reminder>"
        )),
        display: false,
        details: None,
        timestamp: now_millis(),
        rest: serde_json::Map::default(),
    }
}

/// The durable, displayed row recording a mode change.
#[must_use]
pub fn ultracode_change_row(enabled: bool) -> CustomMessage {
    let text = if enabled {
        "Ultracode on: every substantive task runs as a multi-agent rlm.workflow; token cost is not a constraint (/ultracode off to stop)."
    } else {
        "Ultracode off: workflows run only when asked for."
    };
    CustomMessage {
        custom_type: ULTRACODE_CHANGE_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(text.to_string()),
        display: true,
        details: Some(serde_json::json!({ "enabled": enabled })),
        timestamp: now_millis(),
        rest: serde_json::Map::default(),
    }
}

fn change_enabled(custom_type: &str, details: Option<&serde_json::Value>) -> Option<bool> {
    if custom_type != ULTRACODE_CHANGE_CUSTOM_TYPE {
        return None;
    }
    details?.get("enabled")?.as_bool()
}

/// The newest ultracode state along a branch's entries (oldest first).
#[must_use]
pub fn ultracode_in_entries<'a>(
    entries: impl DoubleEndedIterator<Item = &'a FileEntry>,
) -> Option<bool> {
    entries.rev().find_map(|entry| match entry {
        FileEntry::CustomMessage { payload, .. } => {
            change_enabled(&payload.custom_type, payload.details.as_ref())
        }
        FileEntry::Message {
            message: AgentMessage::Custom(message),
            ..
        } => change_enabled(&message.custom_type, message.details.as_ref()),
        _ => None,
    })
}

/// One `/ultracode` invocation (`/effort ultracode [on|off]` arrives as one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UltracodeCommand {
    On,
    Off,
    Status,
}

/// Parse `/ultracode [on|off|status]`; bare `/ultracode` turns it on (like
/// `/effort ultracode`).
///
/// # Errors
///
/// Returns the usage text for any other argument.
pub fn parse_ultracode_command(args: &str) -> Result<UltracodeCommand, String> {
    match args.trim() {
        "" | "on" => Ok(UltracodeCommand::On),
        "off" => Ok(UltracodeCommand::Off),
        "status" => Ok(UltracodeCommand::Status),
        _ => Err("Usage: /ultracode [on|off|status]".to_string()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Announce {
    Enter,
    Exit,
}

#[derive(Debug, Default)]
struct Inner {
    restored: bool,
    on: bool,
    announce: Option<Announce>,
    turns_since_reminder: u32,
}

/// One session's ultracode state: the standing mode and its reminder cadence.
#[derive(Debug, Default)]
pub struct UltracodeState {
    agent_dir: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl UltracodeState {
    #[must_use]
    pub fn new(agent_dir: Option<PathBuf>) -> Self {
        Self {
            agent_dir,
            inner: Mutex::default(),
        }
    }

    fn settings(&self) -> UltracodeSettings {
        UltracodeSettings::load(self.agent_dir.as_deref())
    }

    /// Whether the mode still needs its one-time restore from the branch.
    #[must_use]
    pub fn needs_restore(&self) -> bool {
        !self.inner.lock_or_recover().restored
    }

    /// Adopt the branch's newest recorded mode (the settings default when
    /// the branch never changed it), once; a mode that is on announces
    /// itself on the next turn.
    pub fn restore(&self, from_branch: Option<bool>) {
        let standing_default = self.settings().standing_default;
        let mut inner = self.inner.lock_or_recover();
        if inner.restored {
            return;
        }
        inner.restored = true;
        inner.on = from_branch.unwrap_or(standing_default);
        inner.announce = inner.on.then_some(Announce::Enter);
    }

    #[must_use]
    pub fn is_on(&self) -> bool {
        self.inner.lock_or_recover().on
    }

    /// Switch the mode; returns whether it changed. Turning on queues the
    /// full reminder, turning off the exit notice (or withdraws an entry
    /// reminder the model never saw).
    pub fn set(&self, on: bool) -> bool {
        let mut inner = self.inner.lock_or_recover();
        inner.restored = true;
        if inner.on == on {
            return false;
        }
        inner.on = on;
        inner.turns_since_reminder = 0;
        inner.announce = match (on, inner.announce) {
            (true, _) => Some(Announce::Enter),
            (false, Some(Announce::Enter)) => None,
            (false, _) => Some(Announce::Exit),
        };
        true
    }

    /// The hidden rows one admitted user turn carries, given its text as
    /// typed (before skill and template expansion, as Claude Code detects
    /// the keyword).
    #[must_use]
    pub fn turn_rows(&self, typed: &str) -> Vec<CustomMessage> {
        let settings = self.settings();
        let mut rows = Vec::new();
        let on = {
            let mut inner = self.inner.lock_or_recover();
            match inner.announce.take() {
                Some(Announce::Enter) => {
                    inner.turns_since_reminder = 0;
                    rows.push(reminder_row(ULTRACODE_REMINDER_CUSTOM_TYPE, ENTER_TEXT));
                }
                Some(Announce::Exit) => {
                    rows.push(reminder_row(ULTRACODE_EXIT_CUSTOM_TYPE, EXIT_TEXT));
                }
                None if inner.on => {
                    inner.turns_since_reminder += 1;
                    if inner.turns_since_reminder >= TURNS_BETWEEN_REMINDERS {
                        inner.turns_since_reminder = 0;
                        rows.push(reminder_row(ULTRACODE_REMINDER_CUSTOM_TYPE, STILL_ON_TEXT));
                    }
                }
                None => {}
            }
            inner.on
        };
        rows.extend(keyword_row(typed, settings, on));
        rows
    }

    /// The hidden rows a steer folded into the running turn carries: the
    /// keyword reminder only (the standing cadence counts admitted turns).
    #[must_use]
    pub fn steer_rows(&self, typed: &str) -> Vec<CustomMessage> {
        keyword_row(typed, self.settings(), self.is_on())
            .into_iter()
            .collect()
    }
}

/// The keyword reminder, when the prompt has the keyword, detection is on,
/// and the standing mode (which already opted in) is off.
fn keyword_row(typed: &str, settings: UltracodeSettings, on: bool) -> Option<CustomMessage> {
    if on || !settings.keyword_trigger || !has_ultracode_keyword(typed) {
        return None;
    }
    let text = match settings.guideline_text() {
        Some(guideline) => format!("{KEYWORD_TEXT}\n\n{guideline}"),
        None => KEYWORD_TEXT.to_string(),
    };
    Some(reminder_row(ULTRACODE_KEYWORD_CUSTOM_TYPE, &text))
}

#[cfg(test)]
#[path = "ultracode_tests.rs"]
mod tests;
