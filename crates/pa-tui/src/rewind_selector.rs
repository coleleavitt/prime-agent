//! The `/rewind` picker (Claude Code's message selector, Esc Esc): the
//! session's prompts on the active branch, each with what restoring the code
//! to before it would change; Enter on one asks how to restore (code and
//! conversation, conversation, code, summarize from here, never mind).

use serde_json::Value;

use crate::keybindings::KeybindingsManager;
use crate::menu_panel::{MenuSegment, hint_row, key_hint, menu_row, no_match_row, scroll_row};
use crate::theme::{Theme, ThemeColor};
use crate::width::truncate_line;
use crate::{Line, Span};

/// What restoring the code to before one prompt would change (the
/// daemon's `code` row field).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeSummary {
    /// Files a code restore writes back or deletes.
    pub files_changed: Vec<String>,
    pub insertions: u64,
    pub deletions: u64,
    /// Files changed outside the agent since its last edit: left alone.
    pub externally_modified: Vec<String>,
    /// Files whose earlier content was not kept.
    pub unrestorable: Vec<String>,
}

impl CodeSummary {
    fn from_json(code: &Value) -> Self {
        let paths = |key: &str| -> Vec<String> {
            code.get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            item.as_str()
                                .or_else(|| item.get("path").and_then(Value::as_str))
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            files_changed: paths("filesChanged"),
            insertions: code.get("insertions").and_then(Value::as_u64).unwrap_or(0),
            deletions: code.get("deletions").and_then(Value::as_u64).unwrap_or(0),
            externally_modified: paths("externallyModified"),
            unrestorable: paths("unrestorable"),
        }
    }

    /// Whether a code restore would change anything.
    #[must_use]
    pub fn can_restore(&self) -> bool {
        !self.files_changed.is_empty()
    }
}

/// One prompt the session can rewind to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindCheckpoint {
    pub entry_id: String,
    pub text: String,
    /// `None` when the session has no file checkpoints.
    pub code: Option<CodeSummary>,
}

impl RewindCheckpoint {
    /// The rows of a `get_rewind_checkpoints` response, oldest first.
    #[must_use]
    pub fn from_response(data: &Value) -> Vec<Self> {
        data.get("checkpoints")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        Some(Self {
                            entry_id: row.get("entryId")?.as_str()?.to_string(),
                            text: row.get("text")?.as_str()?.to_string(),
                            code: row.get("code").map(CodeSummary::from_json),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn can_restore_code(&self) -> bool {
        self.code.as_ref().is_some_and(CodeSummary::can_restore)
    }
}

/// How to restore (Claude Code's options, in its order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindOption {
    Both,
    Conversation,
    Code,
    Summarize,
    Nevermind,
}

impl RewindOption {
    fn label(self) -> &'static str {
        match self {
            RewindOption::Both => "Restore code and conversation",
            RewindOption::Conversation => "Restore conversation",
            RewindOption::Code => "Restore code",
            RewindOption::Summarize => "Summarize from here",
            RewindOption::Nevermind => "Never mind",
        }
    }

    /// The option's effect on the conversation.
    fn conversation_effect(self) -> &'static str {
        match self {
            RewindOption::Both | RewindOption::Conversation => {
                "The conversation goes back to before this message; the later turns stay in /tree."
            }
            RewindOption::Summarize => "Messages after this point will be summarized.",
            RewindOption::Code | RewindOption::Nevermind => "The conversation will be unchanged.",
        }
    }

    fn restores_code(self) -> bool {
        matches!(self, RewindOption::Both | RewindOption::Code)
    }
}

/// What a key press did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewindSelectorAction {
    None,
    /// Escape on the prompt list: close.
    Cancel,
    /// A confirmed restore.
    Rewind {
        entry_id: String,
        option: RewindOption,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    List,
    Confirm { option: usize },
}

const MAX_VISIBLE: usize = 10;

/// The `/rewind` picker state.
pub struct RewindSelector {
    checkpoints: Vec<RewindCheckpoint>,
    selected: usize,
    stage: Stage,
}

impl RewindSelector {
    /// Build over the picker rows; the latest prompt is preselected.
    #[must_use]
    pub fn new(checkpoints: Vec<RewindCheckpoint>) -> Self {
        let selected = checkpoints.len().saturating_sub(1);
        Self {
            checkpoints,
            selected,
            stage: Stage::List,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }

    fn options(&self) -> Vec<RewindOption> {
        let mut options = Vec::new();
        if self
            .checkpoints
            .get(self.selected)
            .is_some_and(RewindCheckpoint::can_restore_code)
        {
            options.extend([
                RewindOption::Both,
                RewindOption::Conversation,
                RewindOption::Code,
            ]);
        } else {
            options.push(RewindOption::Conversation);
        }
        options.extend([RewindOption::Summarize, RewindOption::Nevermind]);
        options
    }

    pub fn handle_key(&mut self, kb: &KeybindingsManager, id: &str) -> RewindSelectorAction {
        if self.checkpoints.is_empty() {
            return if kb.matches(id, "tui.select.cancel") || kb.matches(id, "tui.select.confirm") {
                RewindSelectorAction::Cancel
            } else {
                RewindSelectorAction::None
            };
        }
        match self.stage {
            Stage::List => {
                let count = self.checkpoints.len();
                if kb.matches(id, "tui.select.up") {
                    self.selected = (self.selected + count - 1) % count;
                } else if kb.matches(id, "tui.select.down") {
                    self.selected = (self.selected + 1) % count;
                } else if kb.matches(id, "tui.select.confirm") {
                    // Claude Code focuses the fullest restore first.
                    self.stage = Stage::Confirm { option: 0 };
                } else if kb.matches(id, "tui.select.cancel") {
                    return RewindSelectorAction::Cancel;
                }
                RewindSelectorAction::None
            }
            Stage::Confirm { option } => {
                let options = self.options();
                let count = options.len();
                if kb.matches(id, "tui.select.up") {
                    self.stage = Stage::Confirm {
                        option: (option + count - 1) % count,
                    };
                } else if kb.matches(id, "tui.select.down") {
                    self.stage = Stage::Confirm {
                        option: (option + 1) % count,
                    };
                } else if kb.matches(id, "tui.select.confirm") {
                    let chosen = options[option.min(count - 1)];
                    if chosen == RewindOption::Nevermind {
                        self.stage = Stage::List;
                        return RewindSelectorAction::None;
                    }
                    return RewindSelectorAction::Rewind {
                        entry_id: self.checkpoints[self.selected].entry_id.clone(),
                        option: chosen,
                    };
                } else if kb.matches(id, "tui.select.cancel") {
                    self.stage = Stage::List;
                }
                RewindSelectorAction::None
            }
        }
    }

    /// The pane: the prompt list, or the confirmation for the chosen one.
    #[must_use]
    pub fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let mut lines: Vec<Line> = vec![Vec::new(), vec![Span::raw(" Rewind")]];
        match self.stage {
            Stage::List => self.render_list(theme, width, &mut lines),
            Stage::Confirm { option } => self.render_confirm(theme, width, option, &mut lines),
        }
        lines.push(Vec::new());
        lines.push(hint_row(theme, width, &hint(kb)));
        lines
    }

    fn render_list(&self, theme: &Theme, width: usize, lines: &mut Vec<Line>) {
        let has_code = self
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.code.is_some());
        let description = if has_code {
            "Restore the code and/or conversation to the point before\u{2026}"
        } else {
            "Restore the conversation to the point before\u{2026}"
        };
        lines.push(muted_row(theme, width, description));
        lines.push(Vec::new());
        if self.checkpoints.is_empty() {
            lines.push(no_match_row(theme, width, "Nothing to rewind to yet."));
            return;
        }
        let start = self
            .selected
            .saturating_sub(MAX_VISIBLE / 2)
            .min(self.checkpoints.len().saturating_sub(MAX_VISIBLE));
        let end = (start + MAX_VISIBLE).min(self.checkpoints.len());
        for position in start..end {
            let checkpoint = &self.checkpoints[position];
            let summary = checkpoint.code.as_ref().map(row_summary);
            let trailing: Vec<MenuSegment<'_>> = summary
                .as_ref()
                .map(|(text, color)| {
                    vec![match color {
                        Some(color) => MenuSegment::themed(*color, text),
                        None => MenuSegment::muted(text),
                    }]
                })
                .unwrap_or_default();
            lines.push(menu_row(
                theme,
                width,
                vec![Span::raw(one_line(&checkpoint.text))],
                &trailing,
                position == self.selected,
            ));
        }
        if start > 0 || end < self.checkpoints.len() {
            lines.push(scroll_row(
                theme,
                width,
                self.selected + 1,
                self.checkpoints.len(),
            ));
        }
    }

    fn render_confirm(&self, theme: &Theme, width: usize, option: usize, lines: &mut Vec<Line>) {
        let checkpoint = &self.checkpoints[self.selected];
        let options = self.options();
        let focused = options[option.min(options.len() - 1)];
        let code_restorable = checkpoint.can_restore_code();
        let what = if code_restorable {
            ""
        } else {
            "the conversation "
        };
        lines.push(muted_row(
            theme,
            width,
            &format!(
                "Confirm you want to restore {what}to the point before you sent this message:"
            ),
        ));
        let quote = vec![
            Span::raw(" "),
            theme.fg_span(ThemeColor::Dim, "\u{2502} ".to_string()),
            Span::raw(one_line(&checkpoint.text)),
        ];
        lines.push(truncate_line(&quote, width, "\u{2026}"));
        lines.push(muted_row(theme, width, focused.conversation_effect()));
        if focused != RewindOption::Summarize && focused != RewindOption::Nevermind {
            let code = match &checkpoint.code {
                Some(code) if focused.restores_code() && code.can_restore() => code_effect(code),
                _ => "The code will be unchanged.".to_string(),
            };
            lines.push(muted_row(theme, width, &code));
        }
        lines.push(Vec::new());
        for (index, candidate) in options.iter().enumerate() {
            lines.push(menu_row(
                theme,
                width,
                vec![Span::raw(candidate.label())],
                &[],
                index == option,
            ));
        }
        if code_restorable {
            lines.push(Vec::new());
            lines.push(warning_row(
                theme,
                width,
                "Rewinding does not affect files edited manually or via bash.",
            ));
        }
        if let Some(code) = &checkpoint.code {
            if !code.externally_modified.is_empty() {
                lines.push(warning_row(
                    theme,
                    width,
                    &format!(
                        "Left untouched, changed outside the agent since: {}",
                        name_list(&code.externally_modified)
                    ),
                ));
            }
            if !code.unrestorable.is_empty() {
                lines.push(warning_row(
                    theme,
                    width,
                    &format!(
                        "Not restorable (no backup kept): {}",
                        name_list(&code.unrestorable)
                    ),
                ));
            }
        }
    }
}

/// A picker row's code summary: Claude Code's `edited.rs +3 -1`,
/// `3 files changed +x -y`, `No code changes`, or a warning when only
/// untouchable files differ.
fn row_summary(code: &CodeSummary) -> (String, Option<ThemeColor>) {
    let stats = format!("+{} -{}", code.insertions, code.deletions);
    match code.files_changed.as_slice() {
        [] if code.externally_modified.is_empty() && code.unrestorable.is_empty() => {
            ("No code changes".to_string(), None)
        }
        [] => (
            "\u{26a0} No code restore".to_string(),
            Some(ThemeColor::Warning),
        ),
        [only] => (format!("{} {stats}", basename(only)), None),
        files => (format!("{} files changed {stats}", files.len()), None),
    }
}

/// Claude Code's "The code will be restored +x -y in a and b." line.
fn code_effect(code: &CodeSummary) -> String {
    let files = match code.files_changed.as_slice() {
        [] => return "The code has not changed (nothing will be restored).".to_string(),
        [only] => basename(only).to_string(),
        [first, second] => format!("{} and {}", basename(first), basename(second)),
        [first, rest @ ..] => format!("{} and {} other files", basename(first), rest.len()),
    };
    format!(
        "The code will be restored +{} -{} in {files}.",
        code.insertions, code.deletions
    )
}

fn name_list(paths: &[String]) -> String {
    paths
        .iter()
        .map(|path| basename(path))
        .collect::<Vec<_>>()
        .join(", ")
}

fn basename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn muted_row(theme: &Theme, width: usize, text: &str) -> Line {
    let line = vec![
        Span::raw(" "),
        theme.fg_span(ThemeColor::Muted, text.to_string()),
    ];
    truncate_line(&line, width, "\u{2026}")
}

fn warning_row(theme: &Theme, width: usize, text: &str) -> Line {
    let line = vec![
        Span::raw(" "),
        theme.fg_span(ThemeColor::Warning, format!("\u{26a0} {text}")),
    ];
    truncate_line(&line, width, "\u{2026}")
}

/// The shared hint-row grammar (an unbound action is omitted).
fn hint(kb: &KeybindingsManager) -> String {
    [
        key_hint(kb, &["tui.select.up", "tui.select.down"], "navigate"),
        key_hint(kb, &["tui.select.confirm"], "continue"),
        key_hint(kb, &["tui.select.cancel"], "cancel"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<String>>()
    .join(" \u{b7} ")
}

#[cfg(test)]
mod tests;
