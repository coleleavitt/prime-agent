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

use std::path::{Path, PathBuf};
use std::{fs, io};

use pa_retrieval::signal::{AppliedEditView, RefinementRecord, Signal, extract_signals};
use serde::Deserialize;

/// Where the cursor file lives relative to the harness dir.
pub const FEEDBACK_TAP_CURSOR_NAME: &str = "feedback_tap.cursor";

/// Where the local-history cursor lives. Separate from the global one
/// so each file advances independently (local-only sessions might
/// append while a global-heavy session is quiet, and vice versa).
pub const FEEDBACK_TAP_LOCAL_CURSOR_NAME: &str = "feedback_tap_local.cursor";

/// A tap over both the global and the local refinement-history JSONLs.
/// A single call to [`FeedbackTap::drain`] drains BOTH files behind
/// independent byte cursors — the signal-extraction logic is identical,
/// so a local rejection that carries a `failure:<fp>` reaches the
/// retrieval learning loop the same way a global one does.
#[derive(Debug, Clone)]
pub struct FeedbackTap {
    /// The global jsonl (admits and rejections of global-scope refinements).
    pub history_path: PathBuf,
    /// Sidecar cursor for the global file.
    pub cursor_path: PathBuf,
    /// The local-scope JSONL mirror written by
    /// `pa_core::refinement::append_local_refinement`. Carries every
    /// per-session admit/reject so the tap sees them; the authoritative
    /// audit still lives in the session jsonl as a custom entry, this
    /// is just the machine-readable mirror.
    pub local_history_path: PathBuf,
    /// Sidecar cursor for the local mirror.
    pub local_cursor_path: PathBuf,
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
            local_history_path: harness_dir.join("local_refinement_history.jsonl"),
            local_cursor_path: harness_dir.join(FEEDBACK_TAP_LOCAL_CURSOR_NAME),
            harness_state_path: harness_dir.join(crate::HARNESS_STATE_FILE_NAME),
        }
    }

    /// Read every record after the last-known cursor (across BOTH the
    /// global and local history files) and return the retrieval
    /// signals they imply. Advances each file's cursor independently
    /// on success. Any malformed record is logged (via `tracing`) and
    /// skipped; the cursor still advances so a single bad line never
    /// jams the tap.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] on a read/cursor-write failure. A missing
    /// file is NOT an error (either the global or the local can be
    /// absent on a fresh account; the tap returns the other file's
    /// signals, or an empty vec when both are absent).
    pub fn drain(&self) -> io::Result<Vec<Signal>> {
        let targeting = load_memory_targeting(&self.harness_state_path);
        let mut records = Vec::new();

        append_new_records_from(&self.history_path, &self.cursor_path, &mut records)?;
        append_new_records_from(
            &self.local_history_path,
            &self.local_cursor_path,
            &mut records,
        )?;

        Ok(extract_signals(records, &targeting))
    }
}

/// Read `history_path` from byte `cursor_path` onward, parse each
/// non-empty line into a `RefinementRecord`, append to `out`, and
/// advance the cursor to `history_path`'s current size on success. A
/// missing `history_path` is a no-op. The cursor is only advanced
/// when the read AND the parse loop complete without IO error — a
/// malformed record STILL advances the cursor because the malformed
/// line is skipped and will be skipped again on the next drain.
fn append_new_records_from(
    history_path: &Path,
    cursor_path: &Path,
    out: &mut Vec<RefinementRecord>,
) -> io::Result<()> {
    let bytes = match fs::read(history_path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let cursor = fs::read_to_string(cursor_path)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if cursor >= bytes.len() {
        return Ok(());
    }
    let tail = &bytes[cursor..];
    for line in tail.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<RawRecord>(line) {
            Ok(raw) => out.push(raw.into_record()),
            Err(err) => tracing::warn!(
                target: "pa_ledger::feedback_tap",
                path = %history_path.display(),
                %err,
                line_bytes = line.len(),
                "skipping malformed refinement_history line"
            ),
        }
    }
    write_cursor_atomic(cursor_path, bytes.len() as u64)
}

fn write_cursor_atomic(path: &Path, offset: u64) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("cursor.tmp");
    fs::write(&tmp, offset.to_string())?;
    fs::rename(&tmp, path)
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
            if fps.is_empty() {
                None
            } else {
                Some((id.clone(), fps))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use pa_retrieval::Outcome;

    use super::*;

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

    /// Guards the 2026-11-07 local-mirror fix: a local-scope rejection
    /// (admitted via `append_local_refinement`, not the global file)
    /// must reach the same `extract_signals` path as a global one.
    #[test]
    fn drain_picks_up_local_refinement_mirror_too() {
        let dir = tempfile::tempdir().unwrap();
        let hd = dir.path();
        write_state_with_memories(
            &hd.join(crate::HARNESS_STATE_FILE_NAME),
            &[("memory_local", vec!["cafefeed"])],
        );
        // One global record (no match) + one local record that DOES match.
        let g = r#"{"id":"g1","summary":"x","appliedEdits":[]}"#;
        write_history(&hd.join("refinement_history.jsonl"), &[g]);
        let l = r#"{"id":"l1","summary":"x","rejectionCause":"gate","ravo":{"missedCriteria":["failure:cafefeed"]},"appliedEdits":[]}"#;
        write_history(&hd.join("local_refinement_history.jsonl"), &[l]);
        let tap = FeedbackTap::new(hd);
        let signals = tap.drain().unwrap();
        assert_eq!(signals.len(), 1, "exactly one Helped from the local file");
        assert_eq!(signals[0].memory_id, "memory_local");
        assert_eq!(signals[0].outcome, Outcome::Helped);
        // Second drain: both cursors past EOF, nothing re-fires.
        assert!(tap.drain().unwrap().is_empty());
    }

    /// Each file's cursor advances independently: a growth on only the
    /// local file must not advance the global cursor (or vice versa).
    #[test]
    fn drain_cursors_advance_independently_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let hd = dir.path();
        write_state_with_memories(&hd.join(crate::HARNESS_STATE_FILE_NAME), &[]);
        let r1 = r#"{"id":"r1","summary":"","appliedEdits":[{"id":"a","title":"supersedes x","content":""}]}"#;
        write_history(&hd.join("refinement_history.jsonl"), &[r1]);
        let tap = FeedbackTap::new(hd);
        assert_eq!(tap.drain().unwrap().len(), 1);
        // Add ONLY to the local file; global cursor must stay put.
        let l1 = r#"{"id":"l1","summary":"","appliedEdits":[{"id":"b","title":"supersedes y","content":""}]}"#;
        write_history(&hd.join("local_refinement_history.jsonl"), &[l1]);
        let second = tap.drain().unwrap();
        assert_eq!(second.len(), 1, "only the local record's signal fires");
        assert_eq!(second[0].memory_id, "y");
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
