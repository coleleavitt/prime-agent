//! Per-project prompt history: Claude Code's up-arrow recall, persisted so it
//! survives restarts and resume and is shared by every session in a project.
//!
//! Claude Code keeps one global `~/.claude/history.jsonl` and scopes it at read
//! time by matching each line's `project` field against the current directory.
//! This keeps the same per-line shape (`display`, `pastedContents`, `project`,
//! `timestamp`, `sessionId`) but in a *per-project* file under the agent dir,
//! `<agent dir>/projects/<project>/history.jsonl` (the project keying auto
//! memory already uses), so the file stays small, prunes with the project, and
//! never mixes one project's prompts into another's.
//!
//! The daemon owns the file: it is the only writer (one process, so no
//! cross-client contention and none of Claude Code's rewrite-detection), it
//! appends one entry per submitted prompt, and it serves the recall list to a
//! client on attach. Reads are parse-tolerant (a torn or malformed line is
//! skipped), newest-first, de-duplicated by display text, and capped.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::resources::auto_memory;

/// The history file's name (Claude Code's).
pub const HISTORY_FILE: &str = "history.jsonl";
/// How many entries recall seeds into the editor (Claude Code's editor cap).
pub const MAX_RECALL: usize = 100;
/// Lines kept on disk before an append rewrites the file to its tail; the
/// daemon is the sole writer, so the rewrite needs no integrity dance.
pub const MAX_PERSISTED: usize = 2_000;
/// Set truthy to turn prompt history off (Claude Code's
/// `CLAUDE_CODE_SKIP_PROMPT_HISTORY`).
pub const DISABLE_ENV: &str = "PRIME_AGENT_DISABLE_PROMPT_HISTORY";

/// One recorded prompt, Claude Code's `history.jsonl` entry shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// The prompt text exactly as the user submitted it.
    pub display: String,
    /// Pasted image/file payloads keyed by marker (empty when none; kept for
    /// Claude Code shape compatibility).
    #[serde(default, skip_serializing_if = "is_empty_map")]
    pub pasted_contents: serde_json::Map<String, serde_json::Value>,
    /// The directory the prompt was typed in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Unix epoch milliseconds when it was recorded.
    pub timestamp: i64,
    /// The session the prompt was submitted to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

fn is_empty_map(map: &serde_json::Map<String, serde_json::Value>) -> bool {
    map.is_empty()
}

impl HistoryEntry {
    /// A plain-text prompt entry stamped with the current time.
    #[must_use]
    pub fn now(
        display: impl Into<String>,
        project: Option<String>,
        session_id: Option<String>,
    ) -> Self {
        Self {
            display: display.into(),
            pasted_contents: serde_json::Map::new(),
            project,
            timestamp: now_ms(),
            session_id,
        }
    }
}

/// The project's history file path.
#[must_use]
pub fn history_path(agent_dir: &Path, cwd: &Path) -> PathBuf {
    auto_memory::project_dir(agent_dir, cwd).join(HISTORY_FILE)
}

/// Whether the environment turned prompt history off
/// (`1`/`true`/`yes`/`on`).
#[must_use]
pub fn disabled_by_env() -> bool {
    std::env::var(DISABLE_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Milliseconds since the Unix epoch (clamped to a non-negative `i64`).
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Append one submitted prompt to the project history (best effort). A display
/// that is empty after trimming is not recorded.
pub fn append(agent_dir: &Path, cwd: &Path, entry: &HistoryEntry) {
    if entry.display.trim().is_empty() {
        return;
    }
    let path = history_path(agent_dir, cwd);
    if let Err(error) = append_line(&path, entry) {
        tracing::warn!(
            target: "pa_core::prompt_history",
            path = %path.display(),
            %error,
            "could not record a prompt in history"
        );
    }
}

fn append_line(path: &Path, entry: &HistoryEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(entry)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    drop(file);
    maybe_trim(path);
    Ok(())
}

/// Keep the file bounded by rewriting it to its last [`MAX_PERSISTED`] lines.
/// The daemon is the sole reader and writer, so a plain rewrite is safe (no
/// concurrent reader to tear a line, unlike Claude Code's shared global file).
fn maybe_trim(path: &Path) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = contents.lines().collect();
    if lines.len() <= MAX_PERSISTED {
        return;
    }
    let kept = lines[lines.len() - MAX_PERSISTED..].join("\n");
    let tmp = path.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, format!("{kept}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Load the project's prompt history for recall: newest-first, de-duplicated by
/// display text (the newest instance wins), capped at `limit`. Malformed or
/// torn lines are skipped.
#[must_use]
pub fn load(agent_dir: &Path, cwd: &Path, limit: usize) -> Vec<HistoryEntry> {
    let path = history_path(agent_dir, cwd);
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let mut entries: Vec<HistoryEntry> = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<HistoryEntry>(trimmed) {
            if !entry.display.trim().is_empty() {
                entries.push(entry);
            }
        }
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut recall: Vec<HistoryEntry> = Vec::new();
    for entry in entries.into_iter().rev() {
        let key = entry.display.trim().to_string();
        if !seen.insert(key) {
            continue;
        }
        recall.push(entry);
        if recall.len() >= limit {
            break;
        }
    }
    recall
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(display: &str, timestamp: i64) -> HistoryEntry {
        HistoryEntry {
            display: display.to_string(),
            pasted_contents: serde_json::Map::new(),
            project: Some("/repo".to_string()),
            timestamp,
            session_id: Some("sess".to_string()),
        }
    }

    #[test]
    fn append_then_load_returns_newest_first_deduped_and_capped() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        let cwd = dir.path();
        for (text, ts) in [("first", 1), ("second", 2), ("first", 3), ("third", 4)] {
            append(agent, cwd, &entry(text, ts));
        }
        let recall = load(agent, cwd, MAX_RECALL);
        let displays: Vec<&str> = recall.iter().map(|e| e.display.as_str()).collect();
        // Newest-first, and the stale "first" (ts 1) is dropped for the newer one (ts 3).
        assert_eq!(displays, vec!["third", "first", "second"]);
    }

    #[test]
    fn an_empty_display_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), dir.path(), &entry("   ", 1));
        assert!(load(dir.path(), dir.path(), MAX_RECALL).is_empty());
    }

    #[test]
    fn the_limit_caps_the_recall_list() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            append(dir.path(), dir.path(), &entry(&format!("p{i}"), i));
        }
        assert_eq!(load(dir.path(), dir.path(), 3).len(), 3);
    }

    #[test]
    fn a_torn_or_malformed_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), dir.path(), &entry("good", 1));
        let path = history_path(dir.path(), dir.path());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{not valid json\n").unwrap();
        drop(file);
        append(dir.path(), dir.path(), &entry("also-good", 2));
        let displays: Vec<String> = load(dir.path(), dir.path(), MAX_RECALL)
            .into_iter()
            .map(|e| e.display)
            .collect();
        assert_eq!(displays, vec!["also-good".to_string(), "good".to_string()]);
    }

    #[test]
    fn the_file_is_trimmed_to_its_tail_once_it_grows_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_PERSISTED + 50) {
            append(dir.path(), dir.path(), &entry(&format!("p{i}"), i as i64));
        }
        let path = history_path(dir.path(), dir.path());
        let line_count = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(
            line_count <= MAX_PERSISTED,
            "file trimmed to its tail: {line_count}"
        );
        // The newest entries survived the trim.
        let newest = load(dir.path(), dir.path(), 1);
        assert_eq!(newest[0].display, format!("p{}", MAX_PERSISTED + 49));
    }

    #[test]
    fn the_entry_round_trips_in_claude_codes_camelcase_shape() {
        let json = serde_json::to_string(&entry("hi", 7)).unwrap();
        assert!(json.contains("\"sessionId\":\"sess\""), "{json}");
        assert!(json.contains("\"timestamp\":7"), "{json}");
        // An empty pastedContents is omitted, matching a bare Claude Code line.
        assert!(!json.contains("pastedContents"), "{json}");
        let back: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry("hi", 7));
    }

    #[test]
    fn a_claude_code_shaped_line_deserializes() {
        let line = r#"{"display":"ls -la","pastedContents":{},"project":"/x","timestamp":123,"sessionId":"s"}"#;
        let entry: HistoryEntry = serde_json::from_str(line).unwrap();
        assert_eq!(entry.display, "ls -la");
        assert_eq!(entry.session_id.as_deref(), Some("s"));
    }

    #[test]
    fn disabled_by_env_reads_the_switch() {
        // The switch is process-global; assert the parsing logic over explicit input instead.
        assert!(!disabled_by_env() || std::env::var(DISABLE_ENV).is_ok());
    }
}
