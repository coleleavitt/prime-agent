//! flyrag-backed learning reranker.
//!
//! Wraps a [`::flyrag::FlyRag`] behind the [`crate::Reranker`] trait, mapping
//! prime-agent's stable `memory_id: String` ids to flyrag's `DocId(u32)` and
//! buffering open offers so [`Reranker::feedback`] can credit any id that
//! appeared in an earlier rerank. Composes above [`crate::Bm25Index`]: BM25
//! produces the top-K candidates, flyrag re-orders them by the learned
//! `base + λ · learned` score.
//!
//! # Persistence
//!
//! The whole reranker — BM25 index + flyrag state + id maps + outstanding
//! offers — round-trips through serde. prime-agent's daemon writes it to
//! `~/.prime/agent/harness/retrieval_state.json` once per turn and reloads
//! on startup (see [`FlyRagReranker::save`] / [`FlyRagReranker::load`]).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;

use ::flyrag::{self as fly, FlyRag, RagParams};
use serde::{Deserialize, Serialize};

use crate::bm25::Bm25Index;
use crate::{Candidate, OfferId, Outcome, Query, Ranked, RerankResult, Reranker, Result};

/// Base-retriever top-K cap: how many candidates BM25 hands to flyrag
/// before the learning layer picks its own top-N. The flyrag paper's
/// `SciFact` gain is at top-K ≈ 10–100 above the base retriever; `32` is
/// a safe middle at prime-agent's corpus scale.
pub const DEFAULT_BASE_TOP_K: usize = 32;

/// The learning reranker.
///
/// # Threading
///
/// Single-writer. flyrag's hot methods take `&mut self`; the daemon should
/// hold the reranker behind a `Mutex`. Reads (`len`, `is_empty`) are also
/// `&self`.
#[derive(Debug, Serialize, Deserialize)]
pub struct FlyRagReranker {
    /// Base retriever.
    bm25: Bm25Index,
    /// Learning layer.
    rag: FlyRag,
    /// `memory_id -> flyrag DocId` (for ties between the two layers).
    forward: HashMap<String, u32>,
    /// `flyrag DocId -> memory_id` (for feedback lookup).
    reverse: HashMap<u32, String>,
    /// `our OfferId -> (flyrag OfferId, memory_ids in that offer)`.
    offers: HashMap<u64, OfferRec>,
    /// Our offer id generator.
    next_offer: u64,
    /// How many BM25 candidates we hand to flyrag per rerank. `None` =
    /// use [`DEFAULT_BASE_TOP_K`].
    base_top_k: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OfferRec {
    fly_offer: u64,
    memory_ids: Vec<String>,
}

impl FlyRagReranker {
    /// Build an empty reranker with the preregistered flyrag params at
    /// `seed`. Base-retriever top-K defaults to [`DEFAULT_BASE_TOP_K`].
    ///
    /// # Errors
    ///
    /// Returns [`crate::RetrievalError::FlyRag`] if flyrag rejects the
    /// params (the prereg ones are known-valid; this errors only in the
    /// test path with a hand-mutated seed).
    pub fn new(seed: u64) -> Result<Self> {
        Self::with_params(RagParams::prereg(seed))
    }

    /// Build with explicit flyrag params.
    ///
    /// # Errors
    ///
    /// Returns [`crate::RetrievalError::FlyRag`] if flyrag rejects the
    /// params.
    pub fn with_params(params: RagParams) -> Result<Self> {
        Ok(Self {
            bm25: Bm25Index::new(),
            rag: FlyRag::new(params)?,
            forward: HashMap::new(),
            reverse: HashMap::new(),
            offers: HashMap::new(),
            next_offer: 0,
            base_top_k: DEFAULT_BASE_TOP_K,
        })
    }

    /// Override the base-retriever top-`K`.
    #[must_use]
    pub fn with_base_top_k(mut self, k: usize) -> Self {
        self.base_top_k = k.max(1);
        self
    }

    /// Fit flyrag's IDF over `corpus` before any memory is added. Must run
    /// before [`Self::add_memory`]; flyrag codes depend on it. Idempotent
    /// if the index is still empty.
    ///
    /// # Errors
    ///
    /// Returns [`crate::RetrievalError::FlyRag`] if flyrag is non-empty or
    /// the corpus is empty.
    pub fn fit_idf<S: AsRef<str>>(&mut self, corpus: &[S]) -> Result<()> {
        Ok(self.rag.fit_idf(corpus)?)
    }

    /// Serialize the reranker to a JSON writer. Used by the daemon's
    /// per-turn persistence tick.
    ///
    /// # Errors
    ///
    /// Returns [`crate::RetrievalError::Json`] on a write error.
    pub fn save<W: Write>(&self, mut writer: W) -> Result<()> {
        serde_json::to_writer(&mut writer, self)?;
        Ok(())
    }

    /// Load from a JSON reader. The result is identical to a reranker
    /// that saw every `add_memory` + `feedback` the saver did, up to byte
    /// equivalence of the queries that follow.
    ///
    /// # Errors
    ///
    /// Returns [`crate::RetrievalError::Json`] on a malformed file.
    pub fn load<R: Read>(reader: R) -> Result<Self> {
        Ok(serde_json::from_reader(reader)?)
    }

    /// Load from a file path, or return `Ok(None)` when it does not
    /// exist (first boot).
    ///
    /// # Errors
    ///
    /// Returns [`crate::RetrievalError::Io`] on any error other than
    /// `NotFound`, or [`crate::RetrievalError::Json`] on a malformed file.
    pub fn load_from_file(path: &Path) -> Result<Option<Self>> {
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(Some(Self::load(file)?))
    }

    /// Atomically persist to a file: write to a sibling tempfile, then
    /// rename over `path`. Matches pa-core's crash-safe write pattern so
    /// a daemon kill mid-save leaves the old file intact.
    ///
    /// # Errors
    ///
    /// Returns [`crate::RetrievalError::Io`] / [`crate::RetrievalError::Json`]
    /// on a write failure.
    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let tmp = path.with_extension("json.tmp");
        {
            let f = std::fs::File::create(&tmp)?;
            self.save(f)?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Introspect the learned weights of one memory.
    #[must_use]
    pub fn doc_weights(&self, memory_id: &str) -> Option<::flyrag::DocWeights> {
        let doc = self.forward.get(memory_id)?;
        self.rag.doc_weights(fly::DocId(*doc))
    }

    /// Base retriever (BM25) read access.
    #[must_use]
    pub fn base(&self) -> &Bm25Index {
        &self.bm25
    }
}

impl Reranker for FlyRagReranker {
    fn rerank(
        &mut self,
        query: &Query,
        candidates: &[Candidate],
        k: usize,
    ) -> Result<RerankResult> {
        if k == 0 || candidates.is_empty() {
            let offer = OfferId(self.next_offer);
            self.next_offer = self.next_offer.wrapping_add(1);
            self.offers.insert(
                offer.0,
                OfferRec {
                    fly_offer: 0,
                    memory_ids: Vec::new(),
                },
            );
            return Ok(RerankResult {
                offer,
                ranked: Vec::new(),
            });
        }

        // Map candidates → flyrag DocIds. Unknown ids are an error (callers
        // should feed only ids they've already added).
        let mapped: Vec<(fly::DocId, f64)> = candidates
            .iter()
            .map(|c| {
                self.forward
                    .get(&c.id)
                    .copied()
                    .map(|d| (fly::DocId(d), c.base_score))
                    .ok_or_else(|| crate::RetrievalError::UnknownMemory(c.id.clone()))
            })
            .collect::<Result<_>>()?;

        let answer = self.rag.rerank(&query.text, &mapped, k)?;

        let ranked: Vec<Ranked> = answer
            .hits
            .iter()
            .filter_map(|hit| {
                self.reverse.get(&hit.doc.0).map(|id| Ranked {
                    id: id.clone(),
                    learned: hit.learned,
                    score: hit.score,
                })
            })
            .collect();

        let offer = OfferId(self.next_offer);
        self.next_offer = self.next_offer.wrapping_add(1);
        self.offers.insert(
            offer.0,
            OfferRec {
                fly_offer: answer.offer.0,
                memory_ids: ranked.iter().map(|r| r.id.clone()).collect(),
            },
        );
        // Bound offer memory: 10× flyrag's own offer window (its default
        // is 10_000) is plenty.
        while self.offers.len() > 10 * self.rag.params().max_offers {
            if let Some((&oldest, _)) = self.offers.iter().min_by_key(|(k, _)| **k) {
                self.offers.remove(&oldest);
            } else {
                break;
            }
        }

        Ok(RerankResult { offer, ranked })
    }

    fn feedback(&mut self, offer: OfferId, memory_id: &str, outcome: Outcome) -> Result<bool> {
        let Some(rec) = self.offers.get(&offer.0) else {
            return Ok(false);
        };
        if !rec.memory_ids.iter().any(|m| m == memory_id) {
            return Ok(false);
        }
        let Some(&doc) = self.forward.get(memory_id) else {
            return Ok(false);
        };
        let fly_outcome = match outcome {
            Outcome::Helped => fly::Outcome::Helped,
            Outcome::Misled => fly::Outcome::Misled,
        };
        match self
            .rag
            .feedback(fly::OfferId(rec.fly_offer), fly::DocId(doc), fly_outcome)
        {
            Ok(_) => Ok(true),
            Err(
                ::flyrag::FlyRagError::UnknownOffer(_)
                | ::flyrag::FlyRagError::NotOffered { .. }
                | ::flyrag::FlyRagError::AlreadyAnswered { .. },
            ) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn add_memory(&mut self, memory_id: &str, text: &str) -> Result<()> {
        if let Some(&doc) = self.forward.get(memory_id) {
            // Re-add: tombstone the old flyrag slot and drop the forward
            // entry; the new text gets a fresh slot below.
            let _ = self.rag.tombstone(fly::DocId(doc))?;
            self.forward.remove(memory_id);
        }
        self.bm25.upsert(memory_id, text);
        let fly_doc = self.rag.add(text)?;
        self.forward.insert(memory_id.to_string(), fly_doc.0);
        self.reverse.insert(fly_doc.0, memory_id.to_string());
        Ok(())
    }

    fn remove_memory(&mut self, memory_id: &str) -> Result<bool> {
        let bm25_hit = self.bm25.tombstone(memory_id);
        let fly_hit = if let Some(&doc) = self.forward.get(memory_id) {
            self.rag.tombstone(fly::DocId(doc))?
        } else {
            false
        };
        Ok(bm25_hit || fly_hit)
    }

    fn len(&self) -> usize {
        self.bm25.len()
    }
}

/// Run the base retriever and the learning reranker as a single call: the
/// pattern the pa-core digest seam will use. Returns the top-K memories,
/// best first, with an offer id for later feedback.
///
/// # Errors
///
/// Returns [`crate::RetrievalError`] when the base retriever or flyrag
/// errors.
pub fn rerank_full(
    reranker: &mut FlyRagReranker,
    query: &Query,
    k: usize,
) -> Result<RerankResult> {
    let base_k = reranker.base_top_k;
    let candidates = reranker.bm25.top_k(query, base_k);
    reranker.rerank(query, &candidates, k)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(seed: u64) -> FlyRagReranker {
        let mut r = FlyRagReranker::new(seed).unwrap();
        r.fit_idf(&[
            "install the dependencies with bun install, not npm install",
            "install the dependencies with npm install and npm ci",
            "the test runner needs a daemon restart to see new code",
            "shutdown --force kills active agents and is destructive",
        ])
        .unwrap();
        for (id, text) in [
            (
                "bun",
                "install the dependencies with bun install, not npm install",
            ),
            (
                "npm",
                "install the dependencies with npm install and npm ci",
            ),
            (
                "daemon",
                "the test runner needs a daemon restart to see new code",
            ),
            (
                "shutdown",
                "shutdown --force kills active agents and is destructive",
            ),
        ] {
            r.add_memory(id, text).unwrap();
        }
        r
    }

    #[test]
    fn end_to_end_feedback_moves_the_learned_signal() {
        // A single `Helped` can't always overcome a large base-score gap
        // (λ·learned is bounded by the readout's prediction-error cap);
        // what MUST happen is the learned component rises off zero for
        // that doc and stays zero for the others.
        let mut r = seeded(7);
        let q = Query::from_text("how do I install the dependencies");
        let first = rerank_full(&mut r, &q, 3).unwrap();
        assert!(!first.ranked.is_empty());
        let target = first.ranked.last().unwrap().id.clone();
        let target_learned_before = first
            .ranked
            .iter()
            .find(|h| h.id == target)
            .map_or(0.0, |h| h.learned);
        assert!(
            target_learned_before.abs() < 1e-6,
            "cold flyrag has learned≈0, got {target_learned_before}"
        );
        assert!(r.feedback(first.offer, &target, Outcome::Helped).unwrap());

        let second = rerank_full(&mut r, &q, 3).unwrap();
        let target_learned_after = second
            .ranked
            .iter()
            .find(|h| h.id == target)
            .map(|h| h.learned)
            .expect("target still ranked");
        assert!(
            target_learned_after > 0.0,
            "Helped moved the learned signal positive: {target_learned_after} ({:?})",
            second.ranked
        );
        // Untouched docs stay at learned=0.
        let other_learned = second
            .ranked
            .iter()
            .filter(|h| h.id != target)
            .map(|h| h.learned)
            .collect::<Vec<_>>();
        assert!(
            other_learned.iter().all(|&l| l.abs() < 1e-6),
            "other docs' learned is still zero: {other_learned:?}"
        );
    }

    #[test]
    fn sustained_helped_can_overtake_a_narrow_base_score_gap() {
        // The base retriever gives two very close candidates on an install
        // query (bun and npm). Feedback should be able to flip them with
        // enough Helped events on the second-place doc. Picking the
        // runner-up rather than the last keeps the base gap within
        // λ·max_learned (0.5·1.0 = 0.5).
        let mut r = seeded(9);
        let q = Query::from_text("how do I install the dependencies");
        let first = rerank_full(&mut r, &q, 3).unwrap();
        assert!(
            first.ranked.len() >= 2,
            "need two close candidates: {:?}",
            first.ranked
        );
        let top_before = first.ranked[0].id.clone();
        let runner_up = first.ranked[1].id.clone();
        let base_gap = first.ranked[0].score - first.ranked[1].score;
        assert!(
            base_gap < 0.5,
            "the test depends on a gap < λ·max_learned (got {base_gap})"
        );
        for _ in 0..30 {
            let offer = rerank_full(&mut r, &q, 3).unwrap();
            if offer.ranked.iter().any(|h| h.id == runner_up) {
                r.feedback(offer.offer, &runner_up, Outcome::Helped).unwrap();
            }
        }
        let after = rerank_full(&mut r, &q, 3).unwrap();
        assert_eq!(
            after.ranked[0].id, runner_up,
            "sustained Helped promoted {runner_up} over {top_before}: {:?}",
            after.ranked
        );
    }

    #[test]
    fn remove_memory_hides_it_from_future_reranks() {
        let mut r = seeded(11);
        let q = Query::from_text("install dependencies");
        let before = rerank_full(&mut r, &q, 4).unwrap();
        let target = before.ranked[0].id.clone();
        assert!(r.remove_memory(&target).unwrap());
        assert!(!r.remove_memory(&target).unwrap());
        let after = rerank_full(&mut r, &q, 4).unwrap();
        assert!(
            after.ranked.iter().all(|h| h.id != target),
            "removed memory is hidden from rerank: {after:?}"
        );
    }

    #[test]
    fn feedback_for_evicted_or_unknown_offer_is_ok_false() {
        let mut r = seeded(13);
        let unknown_offer = OfferId(99_999);
        assert!(!r.feedback(unknown_offer, "bun", Outcome::Helped).unwrap());
        let q = Query::from_text("install");
        let a = rerank_full(&mut r, &q, 2).unwrap();
        assert!(!r.feedback(a.offer, "nonexistent", Outcome::Helped).unwrap());
    }

    #[test]
    fn save_load_roundtrips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retrieval.json");
        let mut r = seeded(5);
        let q = Query::from_text("install");
        let answer = rerank_full(&mut r, &q, 3).unwrap();
        r.feedback(answer.offer, &answer.ranked[0].id, Outcome::Helped)
            .unwrap();
        r.save_to_file(&path).unwrap();

        let mut back = FlyRagReranker::load_from_file(&path).unwrap().unwrap();
        let fresh_q = Query::from_text("install");
        let a = rerank_full(&mut r, &fresh_q, 3).unwrap();
        let b = rerank_full(&mut back, &fresh_q, 3).unwrap();
        let ids =
            |r: &RerankResult| r.ranked.iter().map(|h| h.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&a), ids(&b), "the reloaded reranker produces the same order");
        // doc_weights reports the same thing after reload.
        assert_eq!(
            r.doc_weights(&a.ranked[0].id),
            back.doc_weights(&a.ranked[0].id)
        );
    }

    #[test]
    fn load_from_file_for_a_missing_path_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never.json");
        assert!(FlyRagReranker::load_from_file(&path).unwrap().is_none());
    }
}
