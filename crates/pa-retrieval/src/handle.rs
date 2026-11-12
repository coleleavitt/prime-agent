//! Thread-safe handle around a [`FlyRagReranker`]: the rerank hook the
//! session's harness digest renders with, the memory upserts/removals and
//! feedback the retrieval tick feeds it, and its persisted state.
//!
//! The handle owns the reranker behind a `Mutex`. Every session in the
//! process on the same agent dir shares one handle ([`shared_handle_for`]),
//! so the hook, the tick and the periodic save all touch one instance.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_core::refinement::HarnessEntry;
use pa_core::refinement::ranking::{HarnessQueryTerms, HarnessRerankHook};

use crate::seed::{seed_from_agent_dir, seed_reranker_from_harness_state};
use crate::{Candidate, FlyRagReranker, Outcome, Query, Reranker, RetrievalError};

/// A cross-crate handle to one `FlyRagReranker`. `Clone` cheap (just an
/// `Arc`). Every method serialises through the inner `Mutex`, so the
/// reranker's single-writer invariant is honoured even when the hook, the
/// memory sink and a background feedback tap all touch it from different
/// tasks.
#[derive(Clone)]
pub struct RerankerHandle {
    inner: Arc<Mutex<FlyRagReranker>>,
    /// The sidecar file the handle persists to (`<agent dir>/harness/retrieval_state.json`).
    /// `None` leaves save/load no-ops.
    persist_path: Option<PathBuf>,
}

impl std::fmt::Debug for RerankerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `inner` is intentionally elided (holds a `Mutex<FlyRagReranker>`
        // whose Debug impl is enormous); the handle's useful debug info
        // is just where it persists and how many memories it currently holds.
        f.debug_struct("RerankerHandle")
            .field("persist_path", &self.persist_path)
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl RerankerHandle {
    /// Wrap an owned reranker that persists nowhere (tests).
    #[cfg(test)]
    pub(crate) fn new(reranker: FlyRagReranker) -> Self {
        Self {
            inner: Arc::new(Mutex::new(reranker)),
            persist_path: None,
        }
    }

    /// Construct a handle backed by `path`: load the file if present,
    /// otherwise start empty with the preregistered flyrag params at
    /// `seed`. Any I/O/JSON error short-circuits to an empty reranker so
    /// a session starts even after a truncated save.
    ///
    /// # Errors
    ///
    /// Returns [`RetrievalError`] when flyrag rejects the
    /// seed (should not happen for a valid `u64`).
    pub fn load_or_new(path: PathBuf, seed: u64) -> Result<Self, RetrievalError> {
        let rerank = match FlyRagReranker::load_from_file(&path) {
            Ok(Some(r)) => r,
            Ok(None) | Err(_) => FlyRagReranker::new(seed)?,
        };
        Ok(Self {
            inner: Arc::new(Mutex::new(rerank)),
            persist_path: Some(path),
        })
    }

    /// Persist the current state to `persist_path`. No-op when the path
    /// is unset (test handles).
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] when the atomic write fails.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = self.persist_path.clone() else {
            return Ok(());
        };
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.save_to_file(&path).map_err(|e| match e {
            RetrievalError::Io(err) => err,
            other => io::Error::other(other.to_string()),
        })
    }

    /// Register a newly-admitted memory entry so BM25 and flyrag start
    /// ranking against it. Idempotent: an existing id is reindexed with
    /// the new text (`FlyRagReranker::add_memory` tombstones the prior
    /// slot). Tracing: errors are logged, never propagated.
    pub fn record_memory_upsert(&self, memory_id: &str, text: &str) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(err) = guard.add_memory(memory_id, text) {
            tracing::warn!(target: "pa_core::retrieval", %memory_id, %err,
                "reranker rejected add_memory; skipping");
        }
    }

    /// Record a memory supersede / removal. Idempotent.
    pub fn record_memory_removal(&self, memory_id: &str) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(err) = guard.remove_memory(memory_id) {
            tracing::warn!(target: "pa_core::retrieval", %memory_id, %err,
                "reranker rejected remove_memory; skipping");
        }
    }

    /// Record a cert-stream outcome for a memory with no live offer to
    /// attribute to — a supersede, a RAVO coverage match, any signal that
    /// arrived after the rerank that showed the memory was evicted from
    /// the offer window. Routes to [`Reranker::feedback_detached`],
    /// which trains flyrag on the doc's own KC units.
    pub fn record_feedback_detached(&self, memory_id: &str, outcome: Outcome) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.feedback_detached(memory_id, outcome) {
            Ok(true) => {
                tracing::debug!(
                    target: "pa_core::retrieval",
                    %memory_id,
                    ?outcome,
                    "detached feedback trained flyrag"
                );
            }
            Ok(false) => {
                tracing::trace!(
                    target: "pa_core::retrieval",
                    %memory_id,
                    "detached feedback for unknown memory id; skipping"
                );
            }
            Err(err) => {
                tracing::warn!(
                    target: "pa_core::retrieval",
                    %memory_id,
                    %err,
                    "reranker rejected detached feedback; skipping"
                );
            }
        }
    }

    /// Live memory count (base retriever).
    #[must_use]
    pub fn len(&self) -> usize {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.len()
    }

    /// Admission-time novelty score for `text` in [0, 1]: 1 = nothing
    /// like it stored, 0 = fully familiar. Pair with the RAVO cert
    /// record for an auditable "is this memory new signal" field.
    /// Returns `None` when the lock is held by a poisoned writer or
    /// flyrag's internals error; the caller logs and skips.
    #[must_use]
    pub fn novelty_score(&self, text: &str) -> Option<f64> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.novelty_score(text) {
            Ok(n) => Some(n),
            Err(err) => {
                tracing::debug!(
                    target: "pa_core::retrieval",
                    %err,
                    "novelty_score errored; omitting field from cert record"
                );
                None
            }
        }
    }

    /// Compact the reranker's index, reclaiming tombstoned slots. Run
    /// periodically (every N refinements) from the retrieval
    /// tick — not every settle. Logs the number of slots dropped.
    pub fn compact(&self) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.compact() {
            Ok(0) => {
                tracing::trace!(
                    target: "pa_core::retrieval",
                    "compact: no tombstoned slots to reclaim"
                );
            }
            Ok(dropped) => {
                tracing::info!(
                    target: "pa_core::retrieval",
                    dropped,
                    "compact: reclaimed tombstoned memory slots"
                );
            }
            Err(err) => {
                tracing::warn!(
                    target: "pa_core::retrieval",
                    %err,
                    "compact: flyrag errored; retaining tombstones"
                );
            }
        }
    }

    /// `true` when the reranker holds no memories.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Build the rerank hook pa-core's digest renderer expects. The hook
    /// consults this handle; fallbacks to `None` (native TF-IDF order)
    /// on any lock poison or reranker error.
    #[must_use]
    pub fn as_rerank_hook(&self) -> HarnessRerankHook {
        let handle = self.clone();
        std::sync::Arc::new(move |entries: &[HarnessEntry], terms: &HarnessQueryTerms| {
            if entries.is_empty() || terms.is_empty() {
                return None;
            }
            // Build a Query from the terms the digest already computed.
            // The weighted map IS the query bag; BM25 reads it directly.
            let query = Query {
                text: String::new(),
                terms: terms
                    .iter()
                    .map(|(t, w)| {
                        (
                            t.clone(),
                            #[expect(
                                clippy::cast_possible_truncation,
                                reason = "harness term weights live in a small bounded range; \
                                              the cast to f32 is intentional (BM25 uses f32)"
                            )]
                            {
                                *w as f32
                            },
                        )
                    })
                    .collect(),
            };
            let mut guard = handle
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Rerank only the entries pa-core is already considering
            // (post-filter for `is_enabled`, pre-truncation): their
            // own BM25 scores decide the order.
            let candidates: Vec<Candidate> = entries
                .iter()
                .map(|e| Candidate {
                    id: e.id.clone(),
                    base_score: 1.0,
                })
                .collect();
            match guard.rerank(&query, &candidates, entries.len()) {
                Ok(result) => Some(result.ranked.into_iter().map(|r| r.id).collect()),
                Err(err) => {
                    tracing::debug!(
                        target: "pa_core::retrieval",
                        %err,
                        "rerank hook fell back to native order"
                    );
                    None
                }
            }
        })
    }
}

/// The agent-dir sidecar location prime-agent persists retrieval state to.
#[must_use]
pub fn default_retrieval_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("harness").join("retrieval_state.json")
}

/// Process-wide map of `agent_dir` → shared [`RerankerHandle`]. Every
/// worker that asks for a handle on the same agent dir gets the SAME
/// `Arc<Mutex<FlyRagReranker>>` — mutations serialise through the
/// Mutex, so two workers can't both load, mutate, and save without
/// clobbering each other's updates.
///
/// The map is keyed by the canonical (`std::fs::canonicalize`) agent
/// dir path so `.prime/agent` and `/home/.../.prime/agent` both hit
/// the same entry even if one caller passes a relative path.
///
/// Cross-PROCESS races (two concurrent daemons on the same agent dir,
/// a print-mode session alongside a daemon worker) are still possible;
/// the on-disk `save_to_file` is atomic tmpfile+rename so neither will
/// see a half-written file, but a losing writer still clobbers the
/// other's mutations. Fixing that needs a file lock on the sidecar —
/// follow-up.
fn shared_handles() -> &'static Mutex<HashMap<PathBuf, RerankerHandle>> {
    static HANDLES: std::sync::OnceLock<Mutex<HashMap<PathBuf, RerankerHandle>>> =
        std::sync::OnceLock::new();
    HANDLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get-or-create the shared `RerankerHandle` for `agent_dir`. Every
/// call in this process on the same agent dir returns the SAME inner
/// Mutex — multi-worker safe. A fresh handle that loaded no state is
/// seeded from the memory entries already on disk, so the index matches
/// the harness state from turn one, and the one-shot `targetsFingerprints`
/// backfill runs (sentinel-gated and idempotent, see
/// [`crate::backfill_targets::backfill_once`]), so the feedback loop has
/// targets to credit.
///
/// # Errors
///
/// Returns [`RetrievalError`] when flyrag rejects the
/// seed on a fresh construction.
pub(crate) fn shared_handle_for(agent_dir: &Path) -> Result<RerankerHandle, RetrievalError> {
    let key = std::fs::canonicalize(agent_dir).unwrap_or_else(|_| agent_dir.to_path_buf());
    let mut map = shared_handles()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(h) = map.get(&key) {
        return Ok(h.clone());
    }
    let path = default_retrieval_path(&key);
    let handle = RerankerHandle::load_or_new(path, seed_from_agent_dir(agent_dir))?;
    if handle.is_empty() {
        seed_reranker_from_harness_state(agent_dir, &handle);
    }
    if let Err(err) = crate::backfill_targets::backfill_once(&agent_dir.join("harness")) {
        tracing::warn!(
            target: "pa_daemon::retrieval",
            %err,
            "targetsFingerprints backfill failed; learning loop keeps its current coverage"
        );
    }
    map.insert(key, handle.clone());
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(h: &RerankerHandle) {
        h.record_memory_upsert("alpha", "install the dependencies with bun install");
        h.record_memory_upsert("beta", "install the dependencies with npm install");
        h.record_memory_upsert("gamma", "restart the daemon to see new code");
    }

    #[test]
    fn hook_reorders_against_a_live_reranker() {
        let r = FlyRagReranker::new(7).unwrap();
        let handle = RerankerHandle::new(r);
        seeded(&handle);

        let mut terms = HarnessQueryTerms::new();
        terms.insert("install".to_string(), 1.0);
        let hook = handle.as_rerank_hook();

        // Build entries matching what the digest render would pass in.
        let make = |id: &str, title: &str| HarnessEntry {
            id: id.to_string(),
            kind: pa_core::refinement::RefinementKind::Memory,
            title: title.to_string(),
            content: String::new(),
            path: "memory".to_string(),
            scope: Some(pa_core::refinement::HarnessScope::Global),
            reference: serde_json::Map::default(),
            arguments: serde_json::Map::default(),
            metadata: serde_json::Map::default(),
            source: "test".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            version: 1,
            targets_fingerprints: Vec::new(),
            lane: pa_core::refinement::RefinementLane::Lesson,
            extensions: serde_json::Map::new(),
        };
        let entries = vec![
            make("gamma", "daemon"),
            make("alpha", "bun"),
            make("beta", "npm"),
        ];
        let order = hook(&entries, &terms).expect("reranker returned an order");
        // Install-flavoured docs come first; gamma (daemon) ranks lowest.
        assert_eq!(order.len(), 3);
        assert!(
            order.iter().position(|id| id == "gamma") > order.iter().position(|id| id == "alpha"),
            "gamma ranks below alpha: {order:?}"
        );
    }

    #[test]
    fn load_or_new_builds_a_fresh_handle_when_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retrieval_state.json");
        let handle = RerankerHandle::load_or_new(path.clone(), 3).unwrap();
        assert_eq!(handle.len(), 0);
        seeded(&handle);
        handle.save().unwrap();
        assert!(path.exists());
        let back = RerankerHandle::load_or_new(path, 3).unwrap();
        assert_eq!(back.len(), 3);
    }

    #[test]
    fn shared_handle_is_the_same_arc_for_every_caller_on_the_same_agent_dir() {
        // Two call sites in the same process on the same agent_dir
        // must receive the SAME `Arc<Mutex>` — a mutation through
        // one must be visible to the other. Otherwise concurrent
        // workers race their saves.
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().to_path_buf();
        let a = shared_handle_for(&agent_dir).unwrap();
        a.record_memory_upsert("a", "install the dependencies with bun");
        let b = shared_handle_for(&agent_dir).unwrap();
        // b sees a's mutation: no independent load.
        assert_eq!(b.len(), 1, "shared handle sees a's upsert");
        // Confirm the two share the SAME inner Mutex by checking that
        // a mutation via b shows up in a immediately.
        b.record_memory_upsert("b", "restart the daemon to see new code");
        assert_eq!(a.len(), 2);

        // A different agent_dir gets its OWN handle.
        let other = tempfile::tempdir().unwrap();
        let c = shared_handle_for(other.path()).unwrap();
        assert_eq!(c.len(), 0, "disjoint agent dirs don't share state");
    }

    #[test]
    fn hook_falls_back_to_none_when_terms_are_empty() {
        let handle = RerankerHandle::new(FlyRagReranker::new(1).unwrap());
        seeded(&handle);
        let hook = handle.as_rerank_hook();
        let make = |id: &str| HarnessEntry {
            id: id.to_string(),
            kind: pa_core::refinement::RefinementKind::Memory,
            title: String::new(),
            content: String::new(),
            path: "memory".to_string(),
            scope: Some(pa_core::refinement::HarnessScope::Global),
            reference: serde_json::Map::default(),
            arguments: serde_json::Map::default(),
            metadata: serde_json::Map::default(),
            source: "test".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            version: 1,
            targets_fingerprints: Vec::new(),
            lane: pa_core::refinement::RefinementLane::Lesson,
            extensions: serde_json::Map::new(),
        };
        assert!(hook(&[make("alpha")], &HarnessQueryTerms::new()).is_none());
        assert!(
            hook(&[], &{
                let mut t = HarnessQueryTerms::new();
                t.insert("x".into(), 1.0);
                t
            })
            .is_none()
        );
    }
}
