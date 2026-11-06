//! The per-settle retrieval tick: drain the cert stream for feedback
//! signals, scan new refinement records for memory add/remove events,
//! feed both through the reranker handle, and persist.
//!
//! Called from the worker's settle path (once per settled turn, on the
//! idle side of the turn so it never blocks a user-visible read). A
//! failure in any step is logged and skipped; the tick is best-effort
//! and must never fail a turn.

use std::fs;
use std::io;
use std::path::Path;

use pa_core::refinement::retrieval::RerankerHandle;
use pa_ledger::FeedbackTap;
use pa_retrieval::Outcome;
use serde::Deserialize;

/// Sidecar that records the byte offset in `refinement_history.jsonl`
/// we've already synced to the reranker's index (memory add/remove).
const MEMORY_SYNC_CURSOR: &str = "retrieval_memory_sync.cursor";

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

    if let Err(err) = handle.save() {
        tracing::warn!(
            target: "pa_daemon::retrieval",
            %err,
            "flyrag state save failed"
        );
    }
}

/// Scan the tail of `refinement_history.jsonl` since the sync cursor and
/// call `record_memory_upsert` / `record_memory_removal` for each applied
/// memory edit. Advances the cursor atomically on success.
fn sync_memory_events(harness_dir: &Path, handle: &RerankerHandle) -> io::Result<()> {
    let history_path = harness_dir.join("refinement_history.jsonl");
    let cursor_path = harness_dir.join(MEMORY_SYNC_CURSOR);
    let bytes = match fs::read(&history_path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let cursor = fs::read_to_string(&cursor_path)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if cursor >= bytes.len() {
        return Ok(());
    }
    let tail = &bytes[cursor..];
    let mut applied = 0usize;
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
                Some("create" | "update") => {
                    let combined = if edit.title.is_empty() {
                        edit.content
                    } else {
                        format!("{}\n{}", edit.title, edit.content)
                    };
                    if !combined.is_empty() {
                        handle.record_memory_upsert(&edit.id, &combined);
                        applied += 1;
                    }
                }
                Some("delete") => {
                    handle.record_memory_removal(&edit.id);
                    applied += 1;
                }
                _ => {}
            }
        }
    }
    let offset = bytes.len().to_string();
    let tmp = cursor_path.with_extension("cursor.tmp");
    if let Some(parent) = cursor_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&tmp, &offset)?;
    fs::rename(&tmp, &cursor_path)?;
    if applied > 0 {
        tracing::info!(
            target: "pa_daemon::retrieval",
            applied,
            "synced memory add/remove events into flyrag"
        );
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
    use super::*;
    use pa_core::refinement::retrieval::RerankerHandle;
    use pa_retrieval::FlyRagReranker;

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
        full.push_str(r#"{"id":"r3","appliedEdits":[{"id":"m1","kind":"memory","action":"delete"}]}"#);
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
            &[r#"{"id":"r1","appliedEdits":[{"id":"s1","kind":"skill","action":"create","title":"t","content":"c"}]}"#],
        );
        let handle = RerankerHandle::new(FlyRagReranker::new(7).unwrap());
        sync_memory_events(&harness, &handle).unwrap();
        assert_eq!(handle.len(), 0);
    }
}
