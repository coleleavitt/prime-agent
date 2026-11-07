//! The per-settle retrieval tick: drain the cert stream for feedback
//! signals, scan new refinement records for memory add/remove events,
//! feed both through the reranker handle, and persist.
//!
//! Called from the worker's settle path (once per settled turn, on the
//! idle side of the turn so it never blocks a user-visible read). A
//! failure in any step is logged and skipped; the tick is best-effort
//! and must never fail a turn.

use std::path::Path;
use std::{fs, io};

use pa_core::refinement::retrieval::RerankerHandle;
use pa_ledger::{CorrectionTap, FeedbackTap};
use pa_retrieval::Outcome;
use serde::{Deserialize, Serialize};

/// Sidecar that records the byte offset in `refinement_history.jsonl`
/// (global-scope admits/rejects) we've already synced to the reranker's
/// index (memory add/remove).
const MEMORY_SYNC_CURSOR: &str = "retrieval_memory_sync.cursor";

/// Sidecar for the LOCAL mirror (`local_refinement_history.jsonl`). A
/// separate cursor so local and global refinement streams advance
/// independently — a local-only session shouldn't advance the global
/// cursor past records it didn't see.
const LOCAL_MEMORY_SYNC_CURSOR: &str = "retrieval_memory_sync_local.cursor";

/// Sidecar that records the admission-time novelty score for every
/// memory create/update event the retrieval tick saw. One JSON line
/// per event; append-only; readable without parsing
/// `refinement_history.jsonl`.
const NOVELTY_DIARY: &str = "retrieval_novelty.jsonl";

/// Compact tombstone slots once every N memory events synced.
const COMPACT_EVERY: usize = 128;

#[derive(Debug, Serialize)]
struct NoveltyEntry<'a> {
    /// ISO-8601 UTC millisecond timestamp at tick time. A late-added
    /// field (schema originally had `memory_id` first) — downstream
    /// readers must tolerate its absence on older rows.
    timestamp: String,
    memory_id: &'a str,
    action: &'a str,
    novelty_score: f64,
}

/// Format an iso8601 UTC timestamp for a `NoveltyEntry`. Matches the
/// shape `pa_ledger::js::now_iso` produces so a reader can parse both
/// uniformly.
fn now_iso() -> String {
    pa_ledger::now_iso()
}

/// One settle-time pass: cheap when nothing changed.
pub(crate) fn run(agent_dir: &Path, handle: &RerankerHandle) {
    // Learning feedback (Helped/Misled).
    let harness_dir = agent_dir.join("harness");
    let tap = FeedbackTap::new(&harness_dir);
    match tap.drain() {
        Ok(signals) if !signals.is_empty() => {
            let mut applied = 0usize;
            for signal in signals {
                let outcome = match signal.outcome {
                    Outcome::Helped => pa_retrieval::Outcome::Helped,
                    Outcome::Misled => pa_retrieval::Outcome::Misled,
                };
                // Cert-stream signals have no offer id — the signal is
                // over admitted edits, not over a prior rerank. Route
                // through the detached-feedback path, which trains
                // flyrag on the document's own KC units (not on a
                // query's). The upstream flyrag change
                // `FlyRag::feedback_detached` (rev 2d84d601) is what
                // makes this a real update instead of a silent no-op.
                handle.record_feedback_detached(&signal.memory_id, outcome);
                applied += 1;
            }
            tracing::info!(
                target: "pa_daemon::retrieval",
                applied,
                "fed cert-stream signals into flyrag (detached)"
            );
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(
                target: "pa_daemon::retrieval",
                %err,
                "feedback tap drain failed"
            );
        }
    }

    // Memory add/remove sync from new refinement_history records.
    if let Err(err) = sync_memory_events(&harness_dir, handle) {
        tracing::warn!(
            target: "pa_daemon::retrieval",
            %err,
            "memory-event sync failed"
        );
    }

    // User- and self-correction taps: scan the active session's
    // jsonl + the digest_memories.jsonl sidecar for correction events
    // and feed them through the detached-feedback path.
    if let Some(ctap) = CorrectionTap::for_latest_session(agent_dir) {
        match ctap.drain_user_corrections() {
            Ok(signals) if !signals.is_empty() => {
                for signal in &signals {
                    handle
                        .record_feedback_detached(&signal.memory_id, pa_retrieval::Outcome::Misled);
                }
                tracing::info!(
                    target: "pa_daemon::retrieval",
                    applied = signals.len(),
                    "user-correction signals trained flyrag"
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(target: "pa_daemon::retrieval", %err, "user-correction tap drain failed")
            }
        }
        match ctap.drain_self_corrections() {
            Ok(signals) if !signals.is_empty() => {
                for signal in &signals {
                    handle
                        .record_feedback_detached(&signal.memory_id, pa_retrieval::Outcome::Misled);
                }
                tracing::info!(
                    target: "pa_daemon::retrieval",
                    applied = signals.len(),
                    "self-correction signals trained flyrag"
                );
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(target: "pa_daemon::retrieval", %err, "self-correction tap drain failed")
            }
        }
    }

    if let Err(err) = handle.save() {
        tracing::warn!(
            target: "pa_daemon::retrieval",
            %err,
            "flyrag state save failed"
        );
    }
}

/// Scan the tail of the refinement-history JSONLs since the sync
/// cursors (both the global file AND the local mirror) and call
/// `record_memory_upsert` / `record_memory_removal` for each applied
/// memory edit. Advances each cursor atomically on success. A missing
/// local mirror is a no-op (the account may be global-only or
/// pre-fix); a missing global file is likewise a no-op.
fn sync_memory_events(harness_dir: &Path, handle: &RerankerHandle) -> io::Result<()> {
    sync_one_history(
        &harness_dir.join("refinement_history.jsonl"),
        &harness_dir.join(MEMORY_SYNC_CURSOR),
        &harness_dir.join(NOVELTY_DIARY),
        handle,
    )?;
    sync_one_history(
        &harness_dir.join("local_refinement_history.jsonl"),
        &harness_dir.join(LOCAL_MEMORY_SYNC_CURSOR),
        &harness_dir.join(NOVELTY_DIARY),
        handle,
    )?;
    Ok(())
}

/// Drain one history file (global or local) against its cursor. Pulled
/// out of `sync_memory_events` so both files take the same code path
/// — the only differences are the file + cursor names.
fn sync_one_history(
    history_path: &Path,
    cursor_path: &Path,
    novelty_path: &Path,
    handle: &RerankerHandle,
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
    let mut applied = 0usize;
    let mut novelty_lines: Vec<String> = Vec::new();
    for line in tail.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_slice::<RawRecord>(line) else {
            continue;
        };
        for edit in rec.applied_edits {
            if edit.kind.as_deref() != Some("memory") {
                continue;
            }
            match edit.action.as_deref() {
                Some(action @ ("create" | "update")) => {
                    let combined = if edit.title.is_empty() {
                        edit.content
                    } else {
                        format!("{}\n{}", edit.title, edit.content)
                    };
                    if combined.is_empty() {
                        continue;
                    }
                    // Admission-time novelty BEFORE the upsert — this
                    // scores against the index as it was, not after.
                    if let Some(score) = handle.novelty_score(&combined) {
                        if let Ok(entry) = serde_json::to_string(&NoveltyEntry {
                            timestamp: now_iso(),
                            memory_id: &edit.id,
                            action,
                            novelty_score: score,
                        }) {
                            novelty_lines.push(entry);
                        }
                    }
                    handle.record_memory_upsert(&edit.id, &combined);
                    applied += 1;
                }
                Some("delete") => {
                    handle.record_memory_removal(&edit.id);
                    applied += 1;
                }
                _ => {}
            }
        }
    }
    // Atomically advance the cursor; append the novelty diary (append
    // is idempotent across crashes within the same tick — the cursor
    // advances last).
    if !novelty_lines.is_empty() {
        append_jsonl(novelty_path, &novelty_lines)?;
    }
    let offset = bytes.len().to_string();
    let tmp = cursor_path.with_extension("cursor.tmp");
    if let Some(parent) = cursor_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&tmp, &offset)?;
    fs::rename(&tmp, cursor_path)?;
    if applied > 0 {
        tracing::info!(
            target: "pa_daemon::retrieval",
            applied,
            novelty_entries = novelty_lines.len(),
            "synced memory add/remove events into flyrag"
        );
        // Periodically compact to reclaim tombstone memory. A `0`
        // return is a no-op; a non-zero return logs the drop count.
        if applied.is_multiple_of(COMPACT_EVERY) || applied >= COMPACT_EVERY {
            handle.compact();
        }
    }
    Ok(())
}

/// Append `lines` to `path` as one JSONL line each, newline-terminated.
fn append_jsonl(path: &Path, lines: &[String]) -> io::Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for line in lines {
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRecord {
    #[serde(default)]
    applied_edits: Vec<RawEdit>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEdit {
    #[serde(default)]
    id: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
}

#[cfg(test)]
mod tests {
    use pa_core::refinement::retrieval::RerankerHandle;
    use pa_retrieval::FlyRagReranker;

    use super::*;

    fn write_history(path: &Path, records: &[&str]) {
        let mut body = String::new();
        for r in records {
            body.push_str(r);
            body.push('\n');
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn memory_sync_picks_up_create_and_delete_edits() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("harness");
        fs::create_dir_all(&harness).unwrap();
        let history = harness.join("refinement_history.jsonl");
        write_history(
            &history,
            &[
                r#"{"id":"r1","appliedEdits":[{"id":"m1","kind":"memory","action":"create","title":"t","content":"c"}]}"#,
                r#"{"id":"r2","appliedEdits":[{"id":"m2","kind":"memory","action":"create","title":"","content":"ci"}]}"#,
            ],
        );
        let handle = RerankerHandle::new(FlyRagReranker::new(7).unwrap());
        sync_memory_events(&harness, &handle).unwrap();
        assert_eq!(handle.len(), 2);

        // Append a delete, drain again.
        let mut full = fs::read_to_string(&history).unwrap();
        full.push_str(
            r#"{"id":"r3","appliedEdits":[{"id":"m1","kind":"memory","action":"delete"}]}"#,
        );
        full.push('\n');
        fs::write(&history, full).unwrap();
        sync_memory_events(&harness, &handle).unwrap();
        assert_eq!(handle.len(), 1);
    }

    #[test]
    fn memory_sync_cursor_advances_past_already_seen_records() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("harness");
        fs::create_dir_all(&harness).unwrap();
        let history = harness.join("refinement_history.jsonl");
        let rec = r#"{"id":"r1","appliedEdits":[{"id":"m1","kind":"memory","action":"create","title":"t","content":"c"}]}"#;
        write_history(&history, &[rec]);
        let handle = RerankerHandle::new(FlyRagReranker::new(7).unwrap());
        sync_memory_events(&harness, &handle).unwrap();
        assert_eq!(handle.len(), 1);
        // Second run: no new records → no-op.
        sync_memory_events(&harness, &handle).unwrap();
        assert_eq!(handle.len(), 1);
    }

    #[test]
    fn missing_history_file_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("harness");
        fs::create_dir_all(&harness).unwrap();
        let handle = RerankerHandle::new(FlyRagReranker::new(7).unwrap());
        sync_memory_events(&harness, &handle).unwrap();
        assert_eq!(handle.len(), 0);
    }

    #[test]
    fn non_memory_edits_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("harness");
        fs::create_dir_all(&harness).unwrap();
        let history = harness.join("refinement_history.jsonl");
        write_history(
            &history,
            &[
                r#"{"id":"r1","appliedEdits":[{"id":"s1","kind":"skill","action":"create","title":"t","content":"c"}]}"#,
            ],
        );
        let handle = RerankerHandle::new(FlyRagReranker::new(7).unwrap());
        sync_memory_events(&harness, &handle).unwrap();
        assert_eq!(handle.len(), 0);
    }

    /// Guards the 2026-11-07 fix: every novelty-diary row carries an iso
    /// timestamp so downstream readers can tell historical replay from
    /// live admits.
    #[test]
    fn novelty_diary_entries_carry_a_timestamp_field() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("harness");
        fs::create_dir_all(&harness).unwrap();
        let history = harness.join("refinement_history.jsonl");
        write_history(
            &history,
            &[
                r#"{"id":"r1","appliedEdits":[{"id":"m1","kind":"memory","action":"create","title":"t","content":"c"}]}"#,
            ],
        );
        let handle = RerankerHandle::new(FlyRagReranker::new(7).unwrap());
        sync_memory_events(&harness, &handle).unwrap();
        let diary = fs::read_to_string(harness.join(NOVELTY_DIARY)).unwrap();
        let first_line = diary.lines().next().expect("at least one row");
        let row: serde_json::Value = serde_json::from_str(first_line).unwrap();
        let ts = row["timestamp"].as_str().expect("timestamp is a string");
        // iso format: YYYY-MM-DDTHH:MM:SS.mmmZ — never an empty string.
        assert!(
            ts.len() >= 20 && ts.ends_with('Z'),
            "timestamp shape is iso-8601 UTC millis: {ts}"
        );
        assert_eq!(row["memory_id"], "m1");
        assert_eq!(row["action"], "create");
    }
}
