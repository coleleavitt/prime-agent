//! The session preview (Claude Code's resume-picker preview, `Ctrl+V`): the selected session's
//! recent conversation, read-only, in place of the list. The transcript comes from the session
//! file's tail (the client and the daemon share the machine; a remote row has no local file),
//! so a long session previews its latest exchange without reading the whole file.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use super::{AgentsViewMode, Line, ThemeColor, str_width, truncate_text};

/// How much of the session file's end the preview reads.
const PREVIEW_TAIL_BYTES: u64 = 1024 * 1024;
/// The newest messages the preview keeps.
const PREVIEW_MAX_ITEMS: usize = 200;
/// The rows one message renders at most before it is cut.
const PREVIEW_MESSAGE_MAX_ROWS: usize = 12;

/// Who wrote one previewed line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PreviewRole {
    User,
    Assistant,
    /// A tool call the assistant made (its name and the first line of its code or command).
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PreviewItem {
    pub(super) role: PreviewRole,
    pub(super) text: String,
}

/// The open preview: the row it shows and its loaded conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionPreview {
    /// The previewed row's identity (the open action acts on the selection, which the preview
    /// pins).
    pub(super) identity: String,
    pub(super) title: String,
    /// `<age> · N messages · <branch>`.
    pub(super) footer: String,
    pub(super) items: Vec<PreviewItem>,
    /// Why the conversation is partial or missing (a long session's tail, an unreadable file).
    pub(super) note: Option<String>,
    /// Rows scrolled up from the newest message.
    pub(super) scroll: usize,
}

/// The conversation in the last [`PREVIEW_TAIL_BYTES`] of a session file, oldest first, and
/// whether the read started past the file's beginning.
pub(super) fn load_preview_items(path: &Path) -> std::io::Result<(Vec<PreviewItem>, bool)> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(PREVIEW_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(usize::try_from(length - start).unwrap_or(0));
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    // A read from mid-file starts inside a line: that partial line is dropped.
    let body = if start > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    let mut items: Vec<PreviewItem> = body.lines().flat_map(entry_items).collect();
    let truncated = start > 0 || items.len() > PREVIEW_MAX_ITEMS;
    if items.len() > PREVIEW_MAX_ITEMS {
        items.drain(..items.len() - PREVIEW_MAX_ITEMS);
    }
    Ok((items, truncated))
}

/// The preview lines of one session-file entry: a user message's text, an assistant message's
/// text and tool calls; everything else (tool results, bookkeeping rows) previews nothing.
fn entry_items(line: &str) -> Vec<PreviewItem> {
    let Ok(entry) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    if entry.get("type").and_then(Value::as_str) != Some("message") {
        return Vec::new();
    }
    let Some(message) = entry.get("message") else {
        return Vec::new();
    };
    let role = match message.get("role").and_then(Value::as_str) {
        Some("user") => PreviewRole::User,
        Some("assistant") => PreviewRole::Assistant,
        _ => return Vec::new(),
    };
    let mut items = Vec::new();
    let push_text = |text: &str, items: &mut Vec<PreviewItem>| {
        let text = text.trim();
        if !text.is_empty() {
            items.push(PreviewItem {
                role,
                text: text.to_string(),
            });
        }
    };
    match message.get("content") {
        Some(Value::String(text)) => push_text(text, &mut items),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            push_text(text, &mut items);
                        }
                    }
                    Some("toolCall") => items.push(PreviewItem {
                        role: PreviewRole::Tool,
                        text: tool_call_line(block),
                    }),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    items
}

/// `name: <first line of the code or command>`.
fn tool_call_line(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
    let arguments = block.get("arguments");
    let first = ["code", "command", "path"]
        .into_iter()
        .find_map(|key| {
            arguments
                .and_then(|args| args.get(key))
                .and_then(Value::as_str)
        })
        .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()));
    match first {
        Some(line) => format!("{name}: {line}"),
        None => name.to_string(),
    }
}

impl AgentsViewMode {
    /// Open the preview over the selected session row (`app.agents.preview`): a summary or
    /// remote row has nothing local to show.
    pub(super) fn open_preview(&mut self) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            return;
        };
        if row.kind == super::RowKind::SubagentSummary || row.kind == super::RowKind::Code {
            self.set_status("Select a session to preview");
            return;
        }
        if self.guard_remote_row("preview") {
            return;
        }
        let Some(file) = row
            .summary
            .get("sessionFile")
            .and_then(Value::as_str)
            .filter(|file| !file.is_empty())
        else {
            self.set_status("This session has no saved transcript to preview");
            return;
        };
        let (items, note) = match load_preview_items(Path::new(file)) {
            Ok((items, truncated)) => {
                let note =
                    truncated.then(|| "Showing the latest part of a long session".to_string());
                (items, note)
            }
            Err(error) => (
                Vec::new(),
                Some(format!("Cannot read the session file: {error}")),
            ),
        };
        let messages = row
            .summary
            .get("messageCount")
            .and_then(Value::as_u64)
            .map(|count| format!("{count} messages"));
        let branch = row
            .summary
            .get("gitBranch")
            .and_then(Value::as_str)
            .filter(|branch| !branch.is_empty())
            .map(str::to_string);
        let footer = [
            Some(row.age.clone()).filter(|age| !age.is_empty()),
            messages,
            branch,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" \u{b7} ");
        self.preview = Some(SessionPreview {
            identity: row.identity.clone(),
            title: row.title.clone(),
            footer,
            items,
            note,
            scroll: 0,
        });
        self.actions.push("session_previewed");
    }

    /// One key while the preview is open: the confirm and open keys open the previewed
    /// session, the selection keys scroll, the exit key keeps its double-press meaning, and any
    /// other key closes the preview. Returns whether the key was consumed.
    pub(super) fn handle_preview_key(&mut self, key: &str) -> bool {
        if self.preview.is_none() {
            return false;
        }
        if self.keybindings.matches(key, "app.clear") {
            return false;
        }
        let page = self.page_step() as isize;
        if self.keybindings.matches(key, "tui.select.confirm")
            || self.keybindings.matches(key, "app.agents.open")
        {
            self.preview = None;
            self.open_selected();
            return true;
        }
        let delta = if self.keybindings.matches(key, "tui.select.up") {
            Some(1)
        } else if self.keybindings.matches(key, "tui.select.down") {
            Some(-1)
        } else if self.keybindings.matches(key, "tui.select.pageUp") {
            Some(page)
        } else if self.keybindings.matches(key, "tui.select.pageDown") {
            Some(-page)
        } else if self.keybindings.matches(key, "tui.select.top") {
            Some(isize::MAX)
        } else if self.keybindings.matches(key, "tui.select.bottom") {
            Some(isize::MIN)
        } else {
            None
        };
        match (delta, self.preview.as_mut()) {
            (Some(delta), Some(preview)) => {
                preview.scroll = preview.scroll.saturating_add_signed(delta);
            }
            _ => self.preview = None,
        }
        true
    }

    /// The preview panel in place of the list: the title, the conversation's newest rows that
    /// fit (scrolled up by the preview's offset), and the footer.
    pub(super) fn render_preview(&mut self, width: usize, max_rows: usize) -> Vec<Line> {
        let theme = &self.theme;
        let Some(preview) = self.preview.as_mut() else {
            return Vec::new();
        };
        let inner = width.saturating_sub(4).max(1);
        let mut body: Vec<Line> = Vec::new();
        if let Some(note) = &preview.note {
            body.push(vec![theme.fg(ThemeColor::Dim, truncate_text(note, width))]);
            body.push(vec![]);
        }
        if preview.items.is_empty() && preview.note.is_none() {
            body.push(vec![
                theme.fg(ThemeColor::Dim, "No messages yet".to_string()),
            ]);
        }
        for item in &preview.items {
            let (marker, color) = match item.role {
                PreviewRole::User => ("\u{203a} ", ThemeColor::Accent),
                PreviewRole::Assistant => ("  ", ThemeColor::Text),
                PreviewRole::Tool => ("\u{25b8} ", ThemeColor::Dim),
            };
            let mut rows: Vec<String> = Vec::new();
            for paragraph in item.text.lines() {
                let wrapped = crate::width::wrap_text(paragraph, inner);
                if wrapped.is_empty() {
                    rows.push(String::new());
                }
                rows.extend(wrapped.into_iter().map(|line| {
                    line.into_iter()
                        .map(|span| span.content)
                        .collect::<String>()
                }));
            }
            if rows.len() > PREVIEW_MESSAGE_MAX_ROWS {
                rows.truncate(PREVIEW_MESSAGE_MAX_ROWS);
                rows.push("\u{2026}".to_string());
            }
            for (index, row) in rows.into_iter().enumerate() {
                let lead = if index == 0 { marker } else { "  " };
                body.push(vec![
                    theme.fg(ThemeColor::Dim, lead.to_string()),
                    theme.fg(color, row),
                ]);
            }
            body.push(vec![]);
        }
        // The header and the footer take two rows; the body shows its newest rows that fit.
        let visible = max_rows.saturating_sub(2);
        let max_scroll = body.len().saturating_sub(visible);
        preview.scroll = preview.scroll.min(max_scroll);
        let end = body.len() - preview.scroll;
        let start = end.saturating_sub(visible);
        let title = format!("Preview \u{b7} {}", preview.title);
        let mut lines = vec![vec![
            theme.fg(ThemeColor::Warning, truncate_text(&title, width)),
        ]];
        lines.extend(body.drain(start..end));
        while lines.len() + 1 < max_rows {
            lines.push(vec![]);
        }
        let position = if preview.scroll > 0 {
            format!("  ({} rows above the newest)", preview.scroll)
        } else {
            String::new()
        };
        let footer = format!("{}{position}", preview.footer);
        if str_width(&footer) > 0 && max_rows > 1 {
            lines.push(vec![
                theme.fg(ThemeColor::Dim, truncate_text(&footer, width)),
            ]);
        }
        lines.truncate(max_rows);
        lines
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;

    #[test]
    fn the_preview_reads_user_and_assistant_turns_and_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            r#"{"type":"session","id":"s","timestamp":"t","cwd":"/w"}"#,
            r#"{"type":"message","id":"1","message":{"role":"user","content":"fix the parser"}}"#,
            r#"{"type":"message","id":"2","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"Looking."},{"type":"toolCall","id":"c","name":"python_repl","arguments":{"code":"\nprint(read('a.rs'))\nmore"}}]}}"#,
            r#"{"type":"message","id":"3","message":{"role":"toolResult","content":"output"}}"#,
            r#"{"type":"model_change","id":"4","provider":"p","modelId":"m"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (items, truncated) = load_preview_items(&path).unwrap();
        assert!(!truncated);
        assert_eq!(
            items,
            vec![
                PreviewItem {
                    role: PreviewRole::User,
                    text: "fix the parser".to_string()
                },
                PreviewItem {
                    role: PreviewRole::Assistant,
                    text: "Looking.".to_string()
                },
                PreviewItem {
                    role: PreviewRole::Tool,
                    text: "python_repl: print(read('a.rs'))".to_string()
                },
            ]
        );
    }

    #[test]
    fn a_long_session_previews_its_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let filler = "x".repeat(4096);
        let mut content = String::new();
        for index in 0..400 {
            writeln!(
                content,
                "{{\"type\":\"message\",\"id\":\"{index}\",\"message\":{{\"role\":\"user\",\"content\":\"m{index} {filler}\"}}}}"
            )
            .unwrap();
        }
        std::fs::write(&path, content).unwrap();
        let (items, truncated) = load_preview_items(&path).unwrap();
        assert!(truncated);
        assert!(items.len() <= PREVIEW_MAX_ITEMS);
        assert!(items.last().unwrap().text.starts_with("m399 "));
    }
}
