//! Claude Code's diagnostics delivery: what servers publish waits in a
//! registry until the next delivery point takes it as one
//! `<new-diagnostics>` reminder.
//!
//! - A publish with diagnostics registers them as pending (an empty publish
//!   only clears the file's latest state).
//! - Taking the pending sets deduplicates them (within the take and against
//!   what was already delivered for the file), sorts each file's by
//!   severity, caps them at 10 per file and 30 in all, and remembers what it
//!   delivered (for the last 500 files).
//! - A file the model edits forgets what was delivered and pending for it,
//!   so the next publish is new again.
//!
//! The text is Claude Code's `<new-diagnostics>` attachment, inside a
//! `<system-reminder>`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

use crate::protocol::{Diagnostic, Severity, uri_to_path};

/// Claude Code's per-file cap.
pub const MAX_PER_FILE: usize = 10;
/// Claude Code's per-delivery cap.
pub const MAX_PER_DELIVERY: usize = 30;
/// Files whose delivered diagnostics are remembered.
pub const DELIVERED_FILES: usize = 500;
/// Claude Code's cap on the rendered text.
pub const MAX_TEXT_CHARS: usize = 4_000;

/// One file's diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiagnostics {
    pub uri: String,
    pub diagnostics: Vec<Diagnostic>,
}

/// What one delivery carries: the servers it came from and the files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub servers: Vec<String>,
    pub files: Vec<FileDiagnostics>,
}

impl Delivery {
    #[must_use]
    pub fn count(&self) -> usize {
        self.files.iter().map(|file| file.diagnostics.len()).sum()
    }
}

/// The identity two diagnostics share when they are the same report.
fn key(diagnostic: &Diagnostic) -> String {
    serde_json::json!({
        "message": diagnostic.message,
        "severity": Severity::from_wire(diagnostic.severity).as_str(),
        "range": diagnostic.range,
        "source": diagnostic.source,
        "code": diagnostic.code,
    })
    .to_string()
}

/// One session's diagnostics state.
#[derive(Debug, Default)]
pub struct DiagnosticsRegistry {
    pending: Vec<(String, FileDiagnostics)>,
    delivered: HashMap<String, HashSet<String>>,
    /// Delivered files, oldest first (the memory's eviction order).
    delivered_order: VecDeque<String>,
    /// Each file's most recent publish (empty ones included).
    latest: HashMap<String, Vec<Diagnostic>>,
    /// Each file's publish count: the boundary waits for it to move.
    publishes: HashMap<String, u64>,
}

impl DiagnosticsRegistry {
    /// Record one publish from `server`; `inject` false (the server's
    /// `diagnostics: false`) keeps it out of the reminders.
    pub fn publish(&mut self, server: &str, uri: &str, diagnostics: Vec<Diagnostic>, inject: bool) {
        *self.publishes.entry(uri.to_string()).or_default() += 1;
        self.latest.insert(uri.to_string(), diagnostics.clone());
        // A newer publish replaces what was still pending for the file.
        self.pending.retain(|(_, file)| file.uri != uri);
        if inject && !diagnostics.is_empty() {
            self.pending.push((
                server.to_string(),
                FileDiagnostics {
                    uri: uri.to_string(),
                    diagnostics,
                },
            ));
        }
    }

    /// How many publishes the file has had.
    #[must_use]
    pub fn publish_count(&self, uri: &str) -> u64 {
        self.publishes.get(uri).copied().unwrap_or(0)
    }

    /// The file's most recent publish, if any.
    #[must_use]
    pub fn latest(&self, uri: &str) -> Option<&[Diagnostic]> {
        self.latest.get(uri).map(Vec::as_slice)
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The model edited the file: forget what was delivered and pending.
    pub fn forget_file(&mut self, uri: &str) {
        self.delivered.remove(uri);
        self.delivered_order.retain(|known| known != uri);
        self.pending.retain(|(_, file)| file.uri != uri);
    }

    /// Remember `diagnostics` as delivered for the file (an explicit
    /// `rlm.lsp.diagnostics` read showed them).
    pub fn mark_delivered(&mut self, uri: &str, diagnostics: &[Diagnostic]) {
        for diagnostic in diagnostics {
            self.remember(uri, key(diagnostic));
        }
        self.pending.retain(|(_, file)| file.uri != uri);
    }

    fn remember(&mut self, uri: &str, key: String) {
        if !self.delivered.contains_key(uri) {
            self.delivered_order.push_back(uri.to_string());
            while self.delivered_order.len() > DELIVERED_FILES {
                if let Some(oldest) = self.delivered_order.pop_front() {
                    self.delivered.remove(&oldest);
                }
            }
        }
        self.delivered
            .entry(uri.to_string())
            .or_default()
            .insert(key);
    }

    /// Take everything pending as one delivery (`None` when nothing new).
    pub fn take_pending(&mut self) -> Option<Delivery> {
        if self.pending.is_empty() {
            return None;
        }
        let mut servers: Vec<String> = Vec::new();
        let mut files: Vec<FileDiagnostics> = Vec::new();
        for (server, file) in std::mem::take(&mut self.pending) {
            if !servers.contains(&server) {
                servers.push(server);
            }
            let index = if let Some(index) = files.iter().position(|known| known.uri == file.uri) {
                index
            } else {
                files.push(FileDiagnostics {
                    uri: file.uri.clone(),
                    diagnostics: Vec::new(),
                });
                files.len() - 1
            };
            let delivered = self.delivered.get(&file.uri);
            let mut seen: HashSet<String> = files[index].diagnostics.iter().map(key).collect();
            for diagnostic in file.diagnostics {
                let identity = key(&diagnostic);
                if delivered.is_some_and(|delivered| delivered.contains(&identity))
                    || !seen.insert(identity)
                {
                    continue;
                }
                files[index].diagnostics.push(diagnostic);
            }
        }
        let mut total = 0;
        for file in &mut files {
            file.diagnostics
                .sort_by_key(|diagnostic| Severity::from_wire(diagnostic.severity));
            file.diagnostics.truncate(MAX_PER_FILE);
            file.diagnostics
                .truncate(MAX_PER_DELIVERY.saturating_sub(total));
            total += file.diagnostics.len();
        }
        files.retain(|file| !file.diagnostics.is_empty());
        for file in &files {
            for diagnostic in &file.diagnostics {
                self.remember(&file.uri, key(diagnostic));
            }
        }
        if files.is_empty() {
            return None;
        }
        Some(Delivery { servers, files })
    }
}

/// One diagnostic line, as Claude Code writes it.
#[must_use]
pub fn diagnostic_line(diagnostic: &Diagnostic) -> String {
    let severity = Severity::from_wire(diagnostic.severity);
    let mut line = format!(
        "  {} [Line {}:{}] {}",
        severity.symbol(),
        diagnostic.range.start.line + 1,
        diagnostic.range.start.character + 1,
        diagnostic.message
    );
    if let Some(code) = diagnostic.code.as_deref().filter(|code| !code.is_empty()) {
        let _ = write!(line, " [{code}]");
    }
    if let Some(source) = diagnostic
        .source
        .as_deref()
        .filter(|source| !source.is_empty())
    {
        let _ = write!(line, " ({source})");
    }
    line
}

/// The files' diagnostics as Claude Code lists them: each file by its name,
/// then its lines; capped at 4,000 characters.
#[must_use]
pub fn render_files(files: &[FileDiagnostics]) -> String {
    let text = files
        .iter()
        .map(|file| {
            let name = uri_to_path(&file.uri)
                .and_then(|path| {
                    Path::new(&path)
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                })
                .or_else(|| file.uri.rsplit('/').next().map(str::to_string))
                .unwrap_or_else(|| file.uri.clone());
            let lines = file
                .diagnostics
                .iter()
                .map(diagnostic_line)
                .collect::<Vec<_>>()
                .join("\n");
            format!("{name}:\n{lines}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.chars().count() > MAX_TEXT_CHARS {
        let kept: String = text.chars().take(MAX_TEXT_CHARS - 12).collect();
        return format!("{kept}\u{2026}[truncated]");
    }
    text
}

/// Claude Code's `<new-diagnostics>` attachment text.
#[must_use]
pub fn new_diagnostics_text(files: &[FileDiagnostics]) -> String {
    format!(
        "<new-diagnostics>The following new diagnostic issues were detected:\n\n{}</new-diagnostics>",
        render_files(files)
    )
}

/// The hidden reminder a delivery reaches the model as.
#[must_use]
pub fn reminder(delivery: &Delivery) -> String {
    pa_core::prompts::session_context::system_reminder(&new_diagnostics_text(&delivery.files))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Position, Range};

    fn diagnostic(line: u32, severity: u8, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position { line, character: 4 },
                end: Position { line, character: 9 },
            },
            severity: Some(severity),
            code: None,
            source: None,
            message: message.to_string(),
        }
    }

    #[test]
    fn deliveries_dedup_sort_and_remember() {
        let mut registry = DiagnosticsRegistry::default();
        registry.publish(
            "ts",
            "file:///r/a.ts",
            vec![diagnostic(3, 2, "unused"), diagnostic(1, 1, "type error")],
            true,
        );
        let first = registry.take_pending().expect("a delivery");
        assert_eq!(first.servers, vec!["ts".to_string()]);
        assert_eq!(
            first.files[0]
                .diagnostics
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>(),
            vec!["type error", "unused"]
        );
        assert_eq!(registry.take_pending(), None);
        // The same report again is not new; a new one is.
        registry.publish(
            "ts",
            "file:///r/a.ts",
            vec![diagnostic(1, 1, "type error"), diagnostic(7, 1, "another")],
            true,
        );
        let second = registry.take_pending().expect("the new one");
        assert_eq!(second.count(), 1);
        assert_eq!(second.files[0].diagnostics[0].message, "another");
        // An edit forgets: the old report is new again.
        registry.forget_file("file:///r/a.ts");
        registry.publish(
            "ts",
            "file:///r/a.ts",
            vec![diagnostic(1, 1, "type error")],
            true,
        );
        assert_eq!(registry.take_pending().map(|d| d.count()), Some(1));
        assert_eq!(registry.publish_count("file:///r/a.ts"), 3);
    }

    #[test]
    fn deliveries_cap_per_file_and_in_all_and_skip_uninjected_servers() {
        let mut registry = DiagnosticsRegistry::default();
        for file in 0..4 {
            let uri = format!("file:///r/{file}.rs");
            let many = (0..15)
                .map(|line| diagnostic(line, 1, &format!("e{line}")))
                .collect();
            registry.publish("ra", &uri, many, true);
        }
        registry.publish(
            "quiet",
            "file:///r/q.rs",
            vec![diagnostic(0, 1, "q")],
            false,
        );
        let delivery = registry.take_pending().expect("a delivery");
        assert_eq!(
            delivery
                .files
                .iter()
                .map(|file| file.diagnostics.len())
                .collect::<Vec<_>>(),
            vec![10, 10, 10]
        );
        assert_eq!(registry.latest("file:///r/q.rs").map(<[_]>::len), Some(1));
    }

    #[test]
    fn the_reminder_is_claude_codes_text() {
        let mut with_code = diagnostic(0, 1, "Cannot find name 'x'.");
        with_code.code = Some("2304".to_string());
        with_code.source = Some("typescript".to_string());
        let files = vec![FileDiagnostics {
            uri: "file:///repo/src/a.ts".to_string(),
            diagnostics: vec![with_code, diagnostic(2, 2, "unused")],
        }];
        assert_eq!(
            new_diagnostics_text(&files),
            "<new-diagnostics>The following new diagnostic issues were detected:\n\na.ts:\n  \u{2718} [Line 1:5] Cannot find name 'x'. [2304] (typescript)\n  \u{26a0} [Line 3:5] unused</new-diagnostics>"
        );
    }

    #[test]
    fn long_text_is_truncated() {
        let files = vec![FileDiagnostics {
            uri: "file:///repo/a.ts".to_string(),
            diagnostics: (0..10)
                .map(|line| diagnostic(line, 1, &"x".repeat(600)))
                .collect(),
        }];
        let text = render_files(&files);
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS - 12 + 12);
        assert!(text.ends_with("\u{2026}[truncated]"));
    }
}
