//! Thread-safe handle around a `pa_retrieval::FlyRagReranker` plus the two
//! glue functions the session engine uses to attach it: a rerank hook for
//! the digest render, and a memory-sink pair the refinement apply path
//! calls when entries land or leave the state.
//!
//! The handle owns the reranker behind a `Mutex`. The session engine keeps
//! an `Arc<RerankerHandle>` on its session so the hook, the memory sink,
//! and the periodic save all share one instance.
//!
//! The hook is what pa-core's [`super::ranking::HarnessStatePromptOptions`]
//! reads. The memory sink is what [`super::planner::apply_refinement_proposal`]
//! calls on each `memory`-kind edit. The feedback loop lives elsewhere
//! (pa-ledger's feedback tap), so this module does not touch it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_retrieval::{FlyRagReranker, Query, RerankResult, Reranker, RetrievalError};

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
    /// Wrap an owned reranker. Use [`RerankerHandle::load_or_new`] on the
    /// daemon side to pair construction with disk state.
    #[must_use]
    pub fn new(reranker: FlyRagReranker) -> Self {
        Self {
            inner: Arc::new(Mutex::new(reranker)),
            persist_path: None,
        }
    }

    /// Set the file this handle persists to. The file is written on
    /// [`RerankerHandle::save`] and read on [`RerankerHandle::load_or_new`].
    #[must_use]
    pub fn with_persist_path(mut self, path: PathBuf) -> Self {
        self.persist_path = Some(path);
        self
    }

    /// Construct a handle backed by `path`: load the file if present,
    /// otherwise start empty with the preregistered flyrag params at
    /// `seed`. Any I/O/JSON error short-circuits to an empty reranker so
    /// the daemon boots even after a truncated save.
    ///
    /// # Errors
    ///
    /// Returns [`pa_retrieval::RetrievalError`] when flyrag rejects the
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

    /// Record a verified outcome against an earlier rerank's offer. The
    /// pa-ledger feedback tap funnels cert-stream signals through here.
    pub fn record_feedback(
        &self,
        offer: pa_retrieval::OfferId,
        memory_id: &str,
        outcome: pa_retrieval::Outcome,
    ) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(err) = guard.feedback(offer, memory_id, outcome) {
            tracing::warn!(target: "pa_core::retrieval", %memory_id, %err,
                "reranker rejected feedback; skipping");
        }
    }

    /// Record a cert-stream outcome for a memory with no live offer to
    /// attribute to — a supersede, a RAVO coverage match, any signal that
    /// arrived after the rerank that showed the memory was evicted from
    /// the offer window. Routes to [`pa_retrieval::Reranker::feedback_detached`],
    /// which trains flyrag on the doc's own KC units.
    pub fn record_feedback_detached(
        &self,
        memory_id: &str,
        outcome: pa_retrieval::Outcome,
    ) {
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

    /// Execute a full rerank against the current index.
    ///
    /// # Errors
    ///
    /// Returns [`pa_retrieval::RetrievalError`] when the underlying
    /// flyrag errors (unknown doc, codec failure).
    pub fn rerank(&self, query: &Query, k: usize) -> Result<RerankResult, RetrievalError> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let base_top_k = 32usize.max(k);
        let candidates = guard.base().top_k(query, base_top_k);
        guard.rerank(query, &candidates, k)
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

    /// `true` when the reranker holds no memories.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Build the rerank hook pa-core's digest renderer expects. The hook
    /// consults this handle; fallbacks to `None` (native TF-IDF order)
    /// on any lock poison or reranker error.
    #[must_use]
    pub fn as_rerank_hook(&self) -> super::ranking::HarnessRerankHook {
        let handle = self.clone();
        std::sync::Arc::new(
            move |entries: &[super::HarnessEntry], terms: &super::ranking::HarnessQueryTerms| {
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
                                              the cast to f32 is intentional (BM25 uses f32)",
                                )]
                                { *w as f32 },
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
                let candidates: Vec<pa_retrieval::Candidate> = entries
                    .iter()
                    .map(|e| pa_retrieval::Candidate {
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
            },
        )
    }
}

/// The agent-dir sidecar location prime-agent persists retrieval state to.
#[must_use]
pub fn default_retrieval_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("harness").join("retrieval_state.json")
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

        let mut terms = super::super::ranking::HarnessQueryTerms::new();
        terms.insert("install".to_string(), 1.0);
        let hook = handle.as_rerank_hook();

        // Build entries matching what the digest render would pass in.
        let make = |id: &str, title: &str| super::super::HarnessEntry {
            id: id.to_string(),
            kind: super::super::RefinementKind::Memory,
            title: title.to_string(),
            content: String::new(),
            path: "memory".to_string(),
            scope: Some(super::super::HarnessScope::Global),
            reference: serde_json::Map::default(),
            arguments: serde_json::Map::default(),
            metadata: serde_json::Map::default(),
            source: "test".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            version: 1,
            targets_fingerprints: Vec::new(),
            lane: super::super::RefinementLane::Lesson,
            extensions: serde_json::Map::new(),
        };
        let entries = vec![make("gamma", "daemon"), make("alpha", "bun"), make("beta", "npm")];
        let order = hook(&entries, &terms).expect("reranker returned an order");
        // Install-flavoured docs come first; gamma (daemon) ranks lowest.
        assert_eq!(order.len(), 3);
        assert!(
            order.iter().position(|id| id == "gamma")
                > order.iter().position(|id| id == "alpha"),
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
    fn hook_falls_back_to_none_when_terms_are_empty() {
        let handle = RerankerHandle::new(FlyRagReranker::new(1).unwrap());
        seeded(&handle);
        let hook = handle.as_rerank_hook();
        let make = |id: &str| super::super::HarnessEntry {
            id: id.to_string(),
            kind: super::super::RefinementKind::Memory,
            title: String::new(),
            content: String::new(),
            path: "memory".to_string(),
            scope: Some(super::super::HarnessScope::Global),
            reference: serde_json::Map::default(),
            arguments: serde_json::Map::default(),
            metadata: serde_json::Map::default(),
            source: "test".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            version: 1,
            targets_fingerprints: Vec::new(),
            lane: super::super::RefinementLane::Lesson,
            extensions: serde_json::Map::new(),
        };
        assert!(
            hook(&[make("alpha")], &super::super::ranking::HarnessQueryTerms::new()).is_none()
        );
        assert!(hook(&[], &{
            let mut t = super::super::ranking::HarnessQueryTerms::new();
            t.insert("x".into(), 1.0);
            t
        })
        .is_none());
    }
}
