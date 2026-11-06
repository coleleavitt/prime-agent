//! Learning memory retrieval for prime-agent.
//!
//! prime-agent's refinement pipeline writes memories autonomously and ends up
//! with more than fit in a turn's prompt budget. This crate is the retrieval
//! layer that picks the right ones for each turn: a BM25 base retriever + a
//! flyrag learning re-ranker + the deterministic signals the cert stream
//! already carries (RAVO rejections, supersede events). Nothing is
//! overwritten; unused memories decay back to a neutral prior; updates are
//! bounded by prediction error. The architecture rhymes with the admission
//! gate it sits above: both make bounded updates, both leave a certificate,
//! both treat history as additive.
//!
//! # Composition
//!
//! ```text
//!   query (goal + recent turns)
//!     └── bm25::Bm25::top_k(query, k=24)      -- base recall over all memories
//!            └── flyrag::FlyRagReranker::rerank(query, candidates, k=16)
//!                   └── top-K into the harness digest
//!
//!   cert stream (refinement_history.jsonl)
//!     └── signal::extract_signals(records)
//!            └── (offer_id, memory_id, Helped|Misled)
//!                   └── reranker.feedback(...)
//! ```
//!
//! # Retriever trait
//!
//! [`Reranker`] is the plug point the pa-core digest seam calls. It has two
//! methods: [`Reranker::rerank`] re-orders a base retriever's candidates and
//! returns an offer id; [`Reranker::feedback`] records an outcome against that
//! offer. [`BasicReranker`] (BM25-only, no learning) and [`FlyRagReranker`]
//! (BM25 + flyrag, learning) both satisfy it, so pa-core can be wired behind a
//! `Box<dyn Reranker>` without caring which one is live.

#![warn(missing_docs)]

pub mod bm25;
pub mod flyrag;
pub mod signal;
mod types;

pub use crate::flyrag::FlyRagReranker;
pub use bm25::{BasicReranker, Bm25Index};
pub use signal::{Signal, extract_signals};
pub use types::{Candidate, OfferId, Outcome, Query, Ranked, RerankResult, RetrievalError, Result};

/// The plug point pa-core calls to pick memories for a turn, and the daemon
/// calls to feed outcomes back.
///
/// Implementations are expected to be single-writer (prime-agent's session
/// engine is serial per session). A `Send + Sync` bound lets a daemon worker
/// hold the reranker behind a mutex and share it across the digest-render and
/// cert-tap paths.
pub trait Reranker: Send + Sync {
    /// Re-rank `candidates` (base-retriever (`memory_id`, `score`) pairs) for
    /// `query` and return the top `k`, with an offer id attached that
    /// [`Reranker::feedback`] can later credit. `candidates` must be ordered
    /// best-first by the base retriever's score; the reranker may reorder.
    ///
    /// # Errors
    ///
    /// Returns [`RetrievalError`] when a candidate names an unknown memory
    /// (should not happen if the caller just loaded them from the same
    /// memory store) or when the underlying flyrag/BM25 index errors.
    fn rerank(
        &mut self,
        query: &Query,
        candidates: &[Candidate],
        k: usize,
    ) -> Result<RerankResult>;

    /// Record one outcome for a shown memory in an earlier offer. `Ok(false)`
    /// means the implementation chose not to persist the signal (feedback
    /// was for a long-evicted offer or the implementation is non-learning);
    /// `Ok(true)` means the outcome landed.
    ///
    /// # Errors
    ///
    /// Returns [`RetrievalError`] when the underlying flyrag errors in an
    /// unexpected way (e.g. codec failures during persistence).
    fn feedback(&mut self, offer: OfferId, memory_id: &str, outcome: Outcome) -> Result<bool>;

    /// Record one outcome for a memory without attributing it to a specific
    /// offer — the cert-stream path. Used when the signal came from a
    /// supersede event or a RAVO coverage match that long out-lived the
    /// 10,000-offer window the reranker remembers. Default `Ok(false)` so
    /// non-learning rerankers accept the call silently.
    ///
    /// # Errors
    ///
    /// Returns [`RetrievalError`] when the underlying flyrag rejects the
    /// id for a reason other than "offer forgotten" (e.g. unknown memory).
    fn feedback_detached(&mut self, memory_id: &str, outcome: Outcome) -> Result<bool> {
        let _ = (memory_id, outcome);
        Ok(false)
    }

    /// Add a memory to the index. Idempotent: re-adding an existing id
    /// updates its stored text in-place (the old code is tombstoned, the
    /// new one gets a fresh slot — flyrag's banks survive the swap for
    /// the old slot but a new slot's banks start untouched).
    ///
    /// # Errors
    ///
    /// Returns [`RetrievalError`] when the underlying index errors.
    fn add_memory(&mut self, memory_id: &str, text: &str) -> Result<()>;

    /// Remove a memory from the index (prime-agent supersede). Idempotent:
    /// returns `Ok(false)` for an unknown id.
    ///
    /// # Errors
    ///
    /// Returns [`RetrievalError`] when the underlying index errors.
    fn remove_memory(&mut self, memory_id: &str) -> Result<bool>;

    /// Number of live (non-tombstoned) memories in the index.
    fn len(&self) -> usize;

    /// `true` when the index is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
