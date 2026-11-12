//! The cold-boot seed of a fresh reranker: the memory entries already in
//! the agent dir's harness state, and the flyrag projection seed.

use std::path::Path;

use pa_ledger::HARNESS_STATE_FILE_NAME;

use crate::handle::RerankerHandle;

/// Seed the reranker from the current `harness_state.json` memory entries
/// (cold-boot path). Each memory entry's title + content is added so BM25
/// has recall from turn one. Failures are logged, never fatal.
///
/// After the seed completes the memory-sync cursor is advanced to the
/// current size of `refinement_history.jsonl`, so the retrieval tick
/// doesn't REPLAY every historical applied edit against a BM25 that
/// already contains the exact same text (which collapses every novelty
/// score to 0.0 — see the pre-fix `retrieval_novelty.jsonl` for
/// evidence). Live admits after the seed still produce real novelty
/// scores because their memory isn't in BM25 until the tick's
/// `record_memory_upsert` call fires.
pub(crate) fn seed_reranker_from_harness_state(agent_dir: &Path, handle: &RerankerHandle) {
    let harness_dir = agent_dir.join("harness");
    let state_path = harness_dir.join(HARNESS_STATE_FILE_NAME);
    let Ok(bytes) = std::fs::read(&state_path) else {
        return;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    let Some(memories) = value
        .get("entries")
        .and_then(|e| e.get("memory"))
        .and_then(serde_json::Value::as_object)
    else {
        return;
    };
    let mut count = 0usize;
    for (id, entry) in memories {
        let title = entry.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let content = entry.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if title.is_empty() && content.is_empty() {
            continue;
        }
        let combined = if title.is_empty() {
            content.to_string()
        } else {
            format!("{title}\n{content}")
        };
        handle.record_memory_upsert(id, &combined);
        count += 1;
    }
    tracing::info!(
        target: "pa_daemon::retrieval",
        memory_count = count,
        "seeded flyrag reranker from harness_state.json"
    );

    // Snap the memory-sync cursor to the current file size so the
    // first retrieval tick doesn't re-walk every record and score it
    // against a BM25 that already carries the exact same text. Idempotent:
    // a cursor that already points past the end (or past the current
    // size) is kept; we only snap it FORWARD, never backward.
    let history_path = harness_dir.join("refinement_history.jsonl");
    let cursor_path = harness_dir.join("retrieval_memory_sync.cursor");
    let current_size = std::fs::metadata(&history_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let existing_cursor = std::fs::read_to_string(&cursor_path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if current_size > existing_cursor {
        let tmp = cursor_path.with_extension("cursor.tmp");
        if std::fs::create_dir_all(&harness_dir).is_ok()
            && std::fs::write(&tmp, current_size.to_string()).is_ok()
            && std::fs::rename(&tmp, &cursor_path).is_ok()
        {
            tracing::info!(
                target: "pa_daemon::retrieval",
                from = existing_cursor,
                to = current_size,
                "snapped memory-sync cursor past historical records (post-seed)"
            );
        }
    }
}

/// Stable per-worker seed. Each agent dir gets its own flyrag projection;
/// the seed only matters until a persisted state file exists (then that
/// state's projection wins on load).
pub(crate) fn seed_from_agent_dir(agent_dir: &Path) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let s = agent_dir.to_string_lossy();
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write(s.as_bytes());
    // Mask to avoid the 0 seed (not required, but a non-zero seed is
    // easier to spot in logs).
    hasher.finish() | 1
}

#[cfg(test)]
mod tests {
    use std::fs;

    use pa_ledger::HARNESS_STATE_FILE_NAME;

    use super::seed_reranker_from_harness_state;
    use crate::FlyRagReranker;
    use crate::handle::RerankerHandle;

    /// Guards the 2026-11-07 novelty-cursor fix: after the cold-boot
    /// seed runs, the memory-sync cursor must point past the end of
    /// `refinement_history.jsonl` so the retrieval tick doesn't replay
    /// historical records against a BM25 that already contains their
    /// exact text (which collapsed every novelty score to 0.0).
    #[test]
    fn seed_snaps_the_memory_sync_cursor_to_eof() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path();
        let harness_dir = agent_dir.join("harness");
        fs::create_dir_all(&harness_dir).unwrap();
        // A minimal harness_state.json with one memory entry.
        let state = serde_json::json!({
            "entries": {
                "memory": {
                    "m1": {"id": "m1", "title": "a title", "content": "a body"}
                }
            }
        });
        fs::write(
            harness_dir.join(HARNESS_STATE_FILE_NAME),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        // A refinement history with one applied edit.
        let history_body = "{\"id\":\"r1\",\"appliedEdits\":[]}\n";
        fs::write(harness_dir.join("refinement_history.jsonl"), history_body).unwrap();

        let handle = RerankerHandle::new(FlyRagReranker::new(7).unwrap());
        seed_reranker_from_harness_state(agent_dir, &handle);

        let cursor = fs::read_to_string(harness_dir.join("retrieval_memory_sync.cursor"))
            .expect("cursor written");
        assert_eq!(
            cursor.trim().parse::<usize>().unwrap(),
            history_body.len(),
            "cursor must point past EOF"
        );
    }

    /// The seed never rolls the cursor BACKWARDS: if another run (or an
    /// earlier settle) already advanced the cursor past the current
    /// file size, the seed must leave it alone.
    #[test]
    fn seed_never_moves_the_cursor_backwards() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path();
        let harness_dir = agent_dir.join("harness");
        fs::create_dir_all(&harness_dir).unwrap();
        let state = serde_json::json!({"entries": {"memory": {}}});
        fs::write(
            harness_dir.join(HARNESS_STATE_FILE_NAME),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        fs::write(harness_dir.join("refinement_history.jsonl"), "{}\n").unwrap();
        // Pre-write a cursor FAR past the current file size.
        fs::write(harness_dir.join("retrieval_memory_sync.cursor"), "99999").unwrap();

        let handle = RerankerHandle::new(FlyRagReranker::new(7).unwrap());
        seed_reranker_from_harness_state(agent_dir, &handle);

        let cursor = fs::read_to_string(harness_dir.join("retrieval_memory_sync.cursor")).unwrap();
        assert_eq!(cursor.trim(), "99999", "cursor stayed forward");
    }
}
