//! BM25 base retriever over harness memories.
//!
//! The classical Okapi BM25 ranking function (Robertson & Zaragoza 2009).
//! At prime-agent's scale (hundreds, maybe low thousands of memories) BM25
//! over a per-crate inverted index is microseconds per query and needs no
//! training — it is the right baseline.
//!
//! This module serves two roles: it is the base retriever the flyrag
//! reranker composes over, AND it is a usable [`Reranker`] on its own
//! ([`BasicReranker`]) for the first ship-it-first phase. Switching between
//! BM25-only and BM25+flyrag is a one-line change at the pa-core seam.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{Candidate, OfferId, Outcome, Query, Ranked, RerankResult, Reranker, Result};

/// Lower-cased alphanumeric tokeniser. Same shape as flyrag's internal
/// tokeniser so the two layers agree on what a "term" is.
#[must_use]
pub fn tokenise(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// BM25 inverted index over memory entries.
///
/// Each memory is one document; its text is a concatenation of its title +
/// content, tokenised. The index is a `HashMap<term, HashMap<doc, tf>>`
/// plus per-doc length, all backed by `Vec`s indexed by an internal doc id.
/// Memories can be tombstoned (supersede) without reindexing — the live
/// bitmap filters hits at query time.
///
/// Serializable: the whole index round-trips through serde for daemon-boot
/// persistence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bm25Index {
    /// Memory id → internal doc id.
    ids: HashMap<String, u32>,
    /// Internal doc id → memory id (dense).
    names: Vec<String>,
    /// Per-doc token length (for BM25's length normalisation).
    lengths: Vec<u32>,
    /// Term → per-term postings (doc → tf).
    postings: HashMap<String, HashMap<u32, u32>>,
    /// Running sum of `lengths` for the average-length denominator.
    total_tokens: u64,
    /// Live bitmap: `tombstoned[i] = true` means doc `i` is retired.
    tombstoned: Vec<bool>,
    /// BM25 `k1`. The literature's "typical" value.
    #[serde(default = "default_k1")]
    k1: f32,
    /// BM25 `b`. The literature's "typical" value.
    #[serde(default = "default_b")]
    b: f32,
}

fn default_k1() -> f32 {
    1.5
}

fn default_b() -> f32 {
    0.75
}

impl Bm25Index {
    /// Empty index with the classical `k1 = 1.5`, `b = 0.75`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            k1: default_k1(),
            b: default_b(),
            ..Self::default()
        }
    }

    /// Number of live (non-tombstoned) documents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len() - self.tombstoned.iter().filter(|&&t| t).count()
    }

    /// `true` when no document is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "BM25 length normalisation uses f32 throughout; the cast is as precise as the \
                  entire score, and prime-agent corpora stay well under 2^23 tokens per doc"
    )]
    fn average_length(&self) -> f32 {
        let live = self.len();
        if live == 0 {
            return 0.0;
        }
        let live_tokens: u64 = self
            .names
            .iter()
            .zip(&self.tombstoned)
            .enumerate()
            .filter_map(|(i, (_, &t))| {
                if t {
                    None
                } else {
                    Some(u64::from(self.lengths[i]))
                }
            })
            .sum();
        (live_tokens as f32) / (live as f32)
    }

    /// Add or replace a document. If `memory_id` already exists it is
    /// tombstoned and a fresh id is assigned — simpler than in-place update
    /// and keeps the "ids are stable per text" invariant the flyrag
    /// reranker relies on for feedback attribution.
    pub fn upsert(&mut self, memory_id: &str, text: &str) {
        if let Some(old_doc) = self.ids.get(memory_id).copied() {
            self.tombstone_doc(old_doc);
        }
        let doc = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        self.names.push(memory_id.to_string());
        self.tombstoned.push(false);
        let tokens = tokenise(text);
        let len = u32::try_from(tokens.len()).unwrap_or(u32::MAX);
        self.lengths.push(len);
        self.total_tokens += u64::from(len);
        let mut tf: HashMap<String, u32> = HashMap::new();
        for t in tokens {
            *tf.entry(t).or_insert(0) += 1;
        }
        for (term, count) in tf {
            self.postings.entry(term).or_default().insert(doc, count);
        }
        self.ids.insert(memory_id.to_string(), doc);
    }

    /// Tombstone `memory_id`. Returns `true` when the id existed and
    /// was not already tombstoned, `false` otherwise.
    pub fn tombstone(&mut self, memory_id: &str) -> bool {
        let Some(&doc) = self.ids.get(memory_id) else {
            return false;
        };
        let doc_ix = doc as usize;
        if self.tombstoned.get(doc_ix).copied().unwrap_or(false) {
            return false;
        }
        self.tombstone_doc(doc);
        true
    }

    fn tombstone_doc(&mut self, doc: u32) {
        let idx = doc as usize;
        if idx >= self.tombstoned.len() {
            return;
        }
        if self.tombstoned[idx] {
            return;
        }
        self.tombstoned[idx] = true;
        // The posting entries are not rewritten — we filter at query time.
        self.total_tokens = self
            .total_tokens
            .saturating_sub(u64::from(self.lengths[idx]));
    }

    /// Top-K memories for `query` by BM25, highest score first, tombstoned
    /// documents skipped. Empty-query → empty result.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "BM25 math is f32 throughout; `live_count`, `df_live`, `tf` and `len` fit the \
                  23-bit mantissa for prime-agent-scale corpora (millions of docs is the limit)"
    )]
    pub fn top_k(&self, query: &Query, k: usize) -> Vec<Candidate> {
        if k == 0 || self.is_empty() {
            return Vec::new();
        }
        // The query's term distribution: either the pa-core-supplied
        // recency-weighted bag, or a fallback tokenisation of the raw text.
        let query_terms: HashMap<String, f32> = if query.terms.is_empty() {
            let toks = tokenise(&query.text);
            let mut bag: HashMap<String, f32> = HashMap::new();
            for t in toks {
                *bag.entry(t).or_insert(0.0) += 1.0;
            }
            bag
        } else {
            query.terms.clone()
        };

        if query_terms.is_empty() {
            return Vec::new();
        }

        let avg_len = self.average_length();
        let live_count = self.len() as f32;
        let mut scores: HashMap<u32, f32> = HashMap::new();
        for (term, q_weight) in &query_terms {
            let Some(posting) = self.postings.get(term) else {
                continue;
            };
            // Document frequency restricted to live documents.
            let df_live = posting
                .keys()
                .filter(|&&d| !self.tombstoned.get(d as usize).copied().unwrap_or(false))
                .count() as f32;
            if df_live <= 0.0 {
                continue;
            }
            // Robertson-Spärck Jones IDF, clamped to non-negative.
            let idf = ((live_count - df_live + 0.5) / (df_live + 0.5) + 1.0).ln();
            if idf <= 0.0 {
                continue;
            }
            for (&doc, &tf) in posting {
                if self.tombstoned.get(doc as usize).copied().unwrap_or(false) {
                    continue;
                }
                let tf = tf as f32;
                let len = self.lengths[doc as usize] as f32;
                let norm = 1.0 - self.b + self.b * (len / avg_len.max(1e-6));
                let bm25 = idf * (tf * (self.k1 + 1.0)) / (tf + self.k1 * norm);
                *scores.entry(doc).or_insert(0.0) += q_weight * bm25;
            }
        }

        let mut hits: Vec<(u32, f32)> = scores.into_iter().collect();
        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(k);
        hits.into_iter()
            .map(|(doc, score)| Candidate {
                id: self.names[doc as usize].clone(),
                base_score: f64::from(score),
            })
            .collect()
    }

    /// Memory ids currently in the index (live + tombstoned). Mainly for
    /// debugging / tests.
    pub fn memory_ids(&self) -> impl Iterator<Item = &str> {
        self.ids.keys().map(String::as_str)
    }
}

/// A [`Reranker`] that runs BM25 only — the baseline before flyrag is
/// wired. `feedback` is accepted and discarded. Useful for (a) the
/// ship-it-first phase before flyrag learning kicks in and (b) the
/// control arm when measuring whether flyrag's gains are real.
///
/// Serializable: the inner [`Bm25Index`] survives a daemon restart.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BasicReranker {
    /// Internal BM25 index.
    pub index: Bm25Index,
    #[serde(default)]
    next_offer: u64,
}

impl BasicReranker {
    /// Empty reranker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Reranker for BasicReranker {
    fn rerank(
        &mut self,
        _query: &Query,
        candidates: &[Candidate],
        k: usize,
    ) -> Result<RerankResult> {
        let ranked: Vec<Ranked> = candidates
            .iter()
            .take(k)
            .map(|c| Ranked {
                id: c.id.clone(),
                learned: 0.0,
                score: c.base_score,
            })
            .collect();
        let offer = OfferId(self.next_offer);
        self.next_offer = self.next_offer.wrapping_add(1);
        Ok(RerankResult { offer, ranked })
    }

    fn feedback(&mut self, _offer: OfferId, _memory_id: &str, _outcome: Outcome) -> Result<bool> {
        // The basic reranker records no state; a feedback call is a no-op.
        Ok(false)
    }

    fn add_memory(&mut self, memory_id: &str, text: &str) -> Result<()> {
        self.index.upsert(memory_id, text);
        Ok(())
    }

    fn remove_memory(&mut self, memory_id: &str) -> Result<bool> {
        Ok(self.index.tombstone(memory_id))
    }

    fn len(&self) -> usize {
        self.index.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str) -> Query {
        Query::from_text(text)
    }

    fn seeded() -> Bm25Index {
        let mut idx = Bm25Index::new();
        idx.upsert(
            "bun",
            "install the dependencies with bun install, not npm install",
        );
        idx.upsert(
            "npm",
            "install the dependencies with npm install and npm ci",
        );
        idx.upsert(
            "daemon",
            "the test runner needs a daemon restart to see new code",
        );
        idx.upsert(
            "shutdown",
            "shutdown --force kills active agents and is destructive",
        );
        idx
    }

    #[test]
    fn bm25_ranks_more_relevant_higher() {
        let idx = seeded();
        let hits = idx.top_k(&query("how do I install the dependencies"), 4);
        assert!(!hits.is_empty());
        assert!(
            hits[0].id == "bun" || hits[0].id == "npm",
            "top hit is one of the install docs: {hits:?}"
        );
    }

    #[test]
    fn tombstone_hides_a_memory() {
        let mut idx = seeded();
        assert!(idx.tombstone("bun"));
        assert!(!idx.tombstone("bun"), "idempotent");
        assert!(!idx.tombstone("nope"), "unknown is no-op");
        let hits = idx.top_k(&query("bun install"), 4);
        assert!(hits.iter().all(|h| h.id != "bun"));
    }

    #[test]
    fn upsert_replaces_and_reindexes() {
        let mut idx = seeded();
        idx.upsert(
            "bun",
            "shutdown of the daemon is destructive and kills active agents",
        );
        let hits = idx.top_k(&query("daemon restart"), 4);
        assert!(
            hits.iter().any(|h| h.id == "bun"),
            "re-upserted bun now matches the daemon query: {hits:?}"
        );
        // The old bun posting (which had `install` and `bun install`
        // bigram) is tombstoned; the new bun has neither token, so
        // `bun install` no longer surfaces bun at all.
        let install = idx.top_k(&query("bun install"), 4);
        assert!(install.iter().all(|h| h.id != "bun"), "{install:?}");
    }

    #[test]
    fn basic_reranker_rerank_identity_and_feedback_noop() {
        let mut r = BasicReranker::new();
        r.add_memory("a", "hello world").unwrap();
        r.add_memory("b", "goodbye world").unwrap();
        let cands = vec![
            Candidate {
                id: "a".into(),
                base_score: 2.0,
            },
            Candidate {
                id: "b".into(),
                base_score: 1.0,
            },
        ];
        let result = r.rerank(&query("hello"), &cands, 2).unwrap();
        assert_eq!(result.ranked[0].id, "a");
        assert_eq!(result.ranked[1].id, "b");
        assert_eq!(r.len(), 2);
        // Feedback is a no-op on the basic reranker.
        assert!(!r.feedback(result.offer, "a", Outcome::Helped).unwrap());
    }

    #[test]
    fn index_roundtrips_through_serde() {
        let idx = seeded();
        let json = serde_json::to_string(&idx).unwrap();
        let back: Bm25Index = serde_json::from_str(&json).unwrap();
        let original = idx.top_k(&query("install bun"), 4);
        let reloaded = back.top_k(&query("install bun"), 4);
        let ids = |hits: &[Candidate]| hits.iter().map(|h| h.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&original), ids(&reloaded));
    }
}
