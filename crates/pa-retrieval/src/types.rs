//! Types shared between the trait, both impls, and the signal module.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A memory id as it appears in `harness_state.json::entries.memory`
/// (`HarnessEntry::id`). Stable across a session; survives restarts.
pub type MemoryId = String;

/// The handle of one rerank's results; [`crate::Reranker::feedback`] names
/// it to attribute an outcome to the offer that showed a memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OfferId(pub u64);

/// A verified outcome for one memory in an earlier offer. The
/// [`signal`](crate::signal) module extracts these from the cert stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// The memory was cited approvingly by the reviewer or RAVO ("already
    /// in global memories", "already stored"), or appeared in the prompt of
    /// a turn whose outcome admitted an edit the model specifically named.
    /// Depresses the flyrag "skip" bank → the memory rises next time.
    Helped,
    /// The memory was superseded, called stale by RAVO, or sat in the
    /// prompt of a turn where the user corrected an error the model should
    /// have avoided given that memory. Depresses the "use" bank → it falls.
    Misled,
}

/// The query at digest time: the raw text the base retriever reads and a
/// weighted term map for BM25 scoring. The map keeps the goal/last-N-messages
/// recency weights the pa-core digest already computes, so a word in the
/// current prompt counts more than one two turns back.
#[derive(Debug, Clone)]
pub struct Query {
    /// The flat query text: goal (if any) + newest N user/assistant texts,
    /// joined with newlines. flyrag reads this directly.
    pub text: String,
    /// Weighted term bag: term → weight (recency-weighted). BM25 reads this
    /// as the query's token distribution. If empty, BM25 falls back to
    /// tokenising `text`.
    pub terms: HashMap<String, f32>,
}

impl Query {
    /// Build from just the raw text (no recency weighting). BM25 tokenises
    /// it; flyrag reads it whole.
    #[must_use]
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            terms: HashMap::new(),
        }
    }
}

/// One base-retriever candidate on its way into the re-ranker.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The memory's stable id.
    pub id: MemoryId,
    /// BM25 score (higher = better).
    pub base_score: f64,
}

/// One re-ranked result.
#[derive(Debug, Clone)]
pub struct Ranked {
    /// The memory's id.
    pub id: MemoryId,
    /// The learned component in [-1, 1] (+1 = learned this helps, -1 =
    /// learned this misleads). Zero for a never-feedbacked memory or for
    /// the non-learning reranker.
    pub learned: f32,
    /// Combined score (base + λ × learned) used to order these results.
    pub score: f64,
}

/// A rerank's results plus the offer id feedback should credit.
#[derive(Debug, Clone)]
pub struct RerankResult {
    /// The offer id this rerank carved out. Pass to
    /// [`crate::Reranker::feedback`] with the outcome.
    pub offer: OfferId,
    /// Top-K hits, best first.
    pub ranked: Vec<Ranked>,
}

/// Retrieval errors.
#[derive(Debug, Error)]
pub enum RetrievalError {
    /// A `Candidate::id` was never added to the index.
    #[error("unknown memory id: {0}")]
    UnknownMemory(String),
    /// flyrag's own error.
    #[error("flyrag: {0}")]
    FlyRag(#[from] ::flyrag::FlyRagError),
    /// A persistence I/O error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encoding/decoding of the persistence sidecar.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, RetrievalError>;
