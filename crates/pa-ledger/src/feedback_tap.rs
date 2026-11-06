//! Read the tail of `refinement_history.jsonl` and emit retrieval
//! feedback signals.
//!
//! The daemon calls [`FeedbackTap::drain`] once per settled turn (or on
//! a background tick). It:
//!
//! 1. Opens `~/.prime/agent/harness/refinement_history.jsonl`.
//! 2. Skips over records already processed (tracked by byte offset on a
//!    sidecar `feedback_tap.cursor` file — crash-safe, no parsing).
//! 3. Parses each new record into `pa_retrieval::signal::RefinementRecord`.
//! 4. Reads the current `harness_state.json` for each memory entry's
//!    `targetsFingerprints` so the RAVO extractor can match them.
//! 5. Runs `pa_retrieval::signal::extract_signals` and returns the
//!    emitted [`pa_retrieval::Signal`]s to the caller (pa-daemon loops
//!    them back into the reranker's feedback method).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use pa_retrieval::signal::{AppliedEditView, RefinementRecord, Signal, extract_signals};
use serde::Deserialize;

/// Where the cursor file lives relative to the harness dir.
pub const FEEDBACK_TAP_CURSOR_NAME: &str = "feedback_tap.cursor";

/// A tap over one `refinement_history.jsonl` file.
#[derive(Debug, Clone)]
pub struct FeedbackTap {
    /// The jsonl file to read.
    pub history_path: PathBuf,
    /// Sidecar cursor file (byte offset into `history_path`).
    pub cursor_path: PathBuf,
    /// The `harness_state.json` beside the history file.
    pub harness_state_path: PathBuf,
}

impl FeedbackTap {
    /// Build a tap over the harness dir at `harness_dir`
    /// (typically `~/.prime/agent/harness/`).
    #[must_use]
    pub fn new(harness_dir: &Path) -> Self {
        Self {
            history_path: harness_dir.join("refinement_history.jsonl"),
            cursor_path: harness_dir.join(FEEDBACK_TAP_CURSOR_NAME),
            harness_state_path: harness_dir.join(crate::HARNESS_STATE_FILE_NAME),
        }
    }

    fn read_cursor(&self) -> u64 {
        fs::read_to_string(&self.cursor_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    fn write_cursor(&self, offset: u64) -> io::Result<()> {
        if let Some(parent) = self.cursor_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.cursor_path.with_extension("cursor.tmp");
        fs::write(&tmp, offset.to_string())?;
        fs::rename(&tmp, &self.cursor_path)
    }

    /// Read every record after the last-known cursor and return the
    /// retrieval signals they imply. Advances the cursor on success. Any
    /// malformed record is logged (via `tracing`) and skipped; the cursor
    /// still advances so a single bad line never jams the tap.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] on a read/cursor-write failure. A missing
    /// history file is NOT an error — the tap returns an empty vec and
    /// leaves the cursor at zero (first-boot case).
    pub fn drain(&self) -> io::Result<Vec<Signal>> {
        let bytes = match fs::read(&self.history_path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let cursor = usize::try_from(self.read_cursor()).unwrap_or(0);
        if cursor >= bytes.len() {
            return Ok(Vec::new());
        }
        let tail = &bytes[cursor..];

        let targeting = load_memory_targeting(&self.harness_state_path);

        let mut records = Vec::new();
        for line in tail.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            match serde_json::from_slice::<RawRecord>(line) {
                Ok(raw) => records.push(raw.into_record()),
                Err(err) => tracing::warn!(
                    target: "pa_ledger::feedback_tap",
                    %err,
                    line_bytes = line.len(),
                    "skipping malformed refinement_history line"
                ),
            }
        }

        let signals = extract_signals(records, &targeting);
        let new_offset = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.write_cursor(new_offset)?;
        Ok(signals)
    }
}

/// The subset of a `RefinementResult` on-disk record the signal extractor
/// needs. serde ignores every other field.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRecord {
    #[serde(default)]
    rejection_cause: Option<String>,
    #[serde(default)]
    ravo: RawRavo,
    #[serde(default)]
    applied_edits: Vec<RawEdit>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRavo {
    #[serde(default)]
    missed_criteria: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEdit {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
}

impl RawRecord {
    fn into_record(self) -> RefinementRecord {
        RefinementRecord {
            missed_criteria: self.ravo.missed_criteria,
            rejection_cause: self.rejection_cause,
            applied_edits: self
                .applied_edits
                .into_iter()
                .map(|e| AppliedEditView {
                    id: e.id,
                    title: e.title,
                    content: e.content,
                })
                .collect(),
        }
    }
}

/// Load `(memory_id, Vec<fingerprint>)` from `harness_state.json`'s
/// memory entries. On any read/parse error returns an empty Vec.
fn load_memory_targeting(path: &Path) -> Vec<(String, Vec<String>)> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    let memories = value
        .get("entries")
        .and_then(|e| e.get("memory"))
        .and_then(|m| m.as_object());
    let Some(memories) = memories else {
        return Vec::new();
    };
    memories
        .iter()
        .filter_map(|(id, entry)| {
            let fps = entry
                .get("targetsFingerprints")
                .and_then(|v| v.as_array())?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>();
            if fps.is_empty() { None } else { Some((id.clone(), fps)) }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_retrieval::Outcome;

    fn write_history(path: &Path, records: &[&str]) {
        let mut body = String::new();
        for r in records {
            body.push_str(r);
            body.push('\n');
        }
        fs::write(path, body).unwrap();
    }

    fn write_state_with_memories(path: &Path, memories: &[(&str, Vec<&str>)]) {
        let entries: serde_json::Map<String, serde_json::Value> = memories
            .iter()
            .map(|(id, fps)| {
                (
                    (*id).to_string(),
                    serde_json::json!({
                        "id": id,
                        "kind": "memory",
                        "targetsFingerprints": fps,
                    }),
                )
            })
            .collect();
        let state = serde_json::json!({
            "schema": 3,
            "entries": { "memory": entries },
            "refinements": [],
            "ravo": {},
            "failures": { "failures": {} },
        });
        fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
    }

    #[test]
    fn drain_extracts_ravo_rejection_signals_and_advances_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let hd = dir.path();
        write_state_with_memories(
            &hd.join(crate::HARNESS_STATE_FILE_NAME),
            &[("memory_covers", vec!["deadbeef"])],
        );
        let rec = r#"{"id":"r1","summary":"x","rejectionCause":"gate","ravo":{"missedCriteria":["failure:deadbeef"]},"appliedEdits":[]}"#;
        write_history(&hd.join("refinement_history.jsonl"), &[rec]);
        let tap = FeedbackTap::new(hd);
        let signals = tap.drain().unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].memory_id, "memory_covers");
        assert_eq!(signals[0].outcome, Outcome::Helped);
        // Second drain is empty: cursor advanced.
        assert!(tap.drain().unwrap().is_empty());
    }

    #[test]
    fn drain_extracts_supersede_signals_from_applied_edits() {
        let dir = tempfile::tempdir().unwrap();
        let hd = dir.path();
        write_state_with_memories(&hd.join(crate::HARNESS_STATE_FILE_NAME), &[]);
        let rec = r#"{"id":"r2","summary":"x","appliedEdits":[{"id":"memory_new","title":"fresh lesson; supersedes memory_old","content":""}]}"#;
        write_history(&hd.join("refinement_history.jsonl"), &[rec]);
        let tap = FeedbackTap::new(hd);
        let signals = tap.drain().unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].memory_id, "memory_old");
        assert_eq!(signals[0].outcome, Outcome::Misled);
    }

    #[test]
    fn drain_resumes_from_the_cursor_so_new_records_only_fire_once() {
        let dir = tempfile::tempdir().unwrap();
        let hd = dir.path();
        write_state_with_memories(&hd.join(crate::HARNESS_STATE_FILE_NAME), &[]);
        let r1 = r#"{"id":"r1","summary":"","appliedEdits":[{"id":"a","title":"supersedes x","content":""}]}"#;
        let r2 = r#"{"id":"r2","summary":"","appliedEdits":[{"id":"b","title":"supersedes y","content":""}]}"#;
        let history = hd.join("refinement_history.jsonl");
        write_history(&history, &[r1]);
        let tap = FeedbackTap::new(hd);
        let first = tap.drain().unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].memory_id, "x");

        // Append a record and drain again: only the new one fires.
        let appended = format!("{r1}\n{r2}\n");
        fs::write(&history, appended).unwrap();
        let second = tap.drain().unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].memory_id, "y");
    }

    #[test]
    fn drain_on_a_missing_history_file_is_empty_and_no_cursor_written() {
        let dir = tempfile::tempdir().unwrap();
        let tap = FeedbackTap::new(dir.path());
        assert!(tap.drain().unwrap().is_empty());
        assert!(!tap.cursor_path.exists());
    }

    #[test]
    fn drain_skips_malformed_lines_without_jamming_the_tap() {
        let dir = tempfile::tempdir().unwrap();
        let hd = dir.path();
        write_state_with_memories(&hd.join(crate::HARNESS_STATE_FILE_NAME), &[]);
        let good = r#"{"id":"r1","summary":"","appliedEdits":[{"id":"a","title":"supersedes x","content":""}]}"#;
        let bad = "{not json";
        write_history(&hd.join("refinement_history.jsonl"), &[bad, good]);
        let tap = FeedbackTap::new(hd);
        let signals = tap.drain().unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].memory_id, "x");
    }
}
