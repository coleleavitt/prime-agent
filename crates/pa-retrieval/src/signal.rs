//! Cert-stream → retrieval feedback.
//!
//! The pa-ledger feedback tap reads `refinement_history.jsonl` records one
//! at a time and asks this module to translate them into (`memory_id`,
//! [`Outcome`]) tuples to feed into [`crate::Reranker::feedback`].
//!
//! Two deterministic signal classes are extracted here. Others (reviewer
//! rationale cites a memory by prose, user correction) need a classifier
//! on top; those extractors live in pa-ledger and compose over this
//! module's output.
//!
//! # Classes
//!
//! 1. **RAVO rejection with `failure:<fingerprint>` in `missedCriteria`.**
//!    The referee rejected a proposal because it did not close a recurring
//!    failure. The memories that *do* target that fingerprint should rise
//!    on a similar query next time. Signal: `Helped` for each memory
//!    whose `targetsFingerprints` contained the fingerprint.
//!
//! 2. **Supersede event: an applied edit's title/content says "supersedes <id>".**
//!    The newer entry retires the older. Signal: `Misled` on the predecessor.
//!
//! # Not here (orthogonal)
//!
//! - Reviewer rationale that quotes a memory id by name (needs regex +
//!   classifier). The pa-ledger tap can call a Laya `noul` on this.
//! - User correction after an agent error on a turn whose digest contained
//!   memory M (needs cross-record joins, better done at the daemon level).

use serde::{Deserialize, Serialize};

use crate::Outcome;

/// Retrieval feedback the cert stream yielded.
///
/// `offer_id` is `None` when the signal did not come from a specific
/// rerank (supersede, RAVO rejection on a different turn's memories).
/// The reranker can still feed it in as a loose signal by picking the
/// most recent offer that contained the memory — or by starting a
/// detached training offer (see `pa-ledger::feedback_tap`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    /// The target memory's id.
    pub memory_id: String,
    /// The outcome to credit.
    pub outcome: Outcome,
    /// A short label of why this signal fired (for the daemon log).
    pub cause: SignalCause,
}

/// Why a [`Signal`] was emitted — the audit-trail label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalCause {
    /// A RAVO rejection whose `missedCriteria` named a failure fingerprint
    /// this memory targeted.
    RavoCoverage {
        /// Which fingerprint the referee was looking for.
        fingerprint: String,
    },
    /// The memory was superseded by a later edit whose title or content
    /// quoted the string "supersede".
    Superseded {
        /// The id of the newer memory that superseded this one.
        successor: String,
    },
}

/// Parse a `missedCriteria: ["failure:<hex>"]` list into the raw
/// fingerprint strings (stripping the `failure:` prefix).
#[must_use]
pub fn failure_fingerprints_from_missed(missed: &[String]) -> Vec<String> {
    missed
        .iter()
        .filter_map(|c| c.strip_prefix("failure:").map(str::to_string))
        .collect()
}

/// For a RAVO rejection record. Given the fingerprint(s) the referee
/// missed and the (`memory_id`, fingerprints) pairs from the current
/// harness state, emit one `Helped` signal per memory that targets any
/// missed fingerprint. The caller supplies both sides because the
/// cert record names only the fingerprints, not the memories.
#[must_use]
pub fn signals_from_ravo_rejection<I, J, M, F>(
    missed_fingerprints: I,
    memory_targeting: J,
) -> Vec<Signal>
where
    I: IntoIterator<Item = String>,
    J: IntoIterator<Item = (M, F)>,
    M: AsRef<str>,
    F: IntoIterator<Item = String>,
{
    let missed: std::collections::HashSet<String> = missed_fingerprints.into_iter().collect();
    if missed.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (memory_id, targets) in memory_targeting {
        for fp in targets {
            if missed.contains(&fp) {
                out.push(Signal {
                    memory_id: memory_id.as_ref().to_string(),
                    outcome: Outcome::Helped,
                    cause: SignalCause::RavoCoverage { fingerprint: fp },
                });
                break;
            }
        }
    }
    out
}

/// Scan one applied-edit (`title`, `content`) pair for a supersede event
/// pointing at a predecessor by id, matching strings of the form
/// `supersedes <id>` (case-insensitive, optional preposition). Returns
/// the id(s) that were superseded.
///
/// Patterns matched:
/// - `supersedes memory_foo`
/// - `Supersedes memory_foo`
/// - `supersedes the memory_foo`
/// - `supersedes the 'memory_foo'` (strips a single pair of surrounding
///   single, double, backtick, or curly quotes)
///
/// Prime-agent refiners sometimes write "supersedes the 'not needed'
/// decision" where the predecessor is named in prose rather than by id.
/// That case needs a cross-record id lookup and is left to pa-ledger.
#[must_use]
pub fn superseded_predecessors(title: &str, content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for text in [title, content] {
        scan_supersedes(text, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn scan_supersedes(text: &str, out: &mut Vec<String>) {
    // Case-insensitive sub-string scan. Avoids a `regex` dep: the pattern
    // is small enough.
    let lower = text.to_lowercase();
    let mut cursor = 0;
    while let Some(off) = lower[cursor..].find("supersedes ") {
        let start = cursor + off + "supersedes ".len();
        let tail = &text[start..];
        // Optional "the " prefix.
        let tail = tail.strip_prefix("the ").unwrap_or(tail);
        // Grab the next token up to whitespace or terminal punctuation.
        let token: String = tail
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(*c, ',' | '.' | ';' | '!' | '?' | ')'))
            .collect();
        if !token.is_empty() {
            let stripped = strip_balanced_quotes(&token);
            if looks_like_id(stripped) {
                out.push(stripped.to_string());
            }
        }
        cursor = start;
    }
}

fn strip_balanced_quotes(s: &str) -> &str {
    let pairs = [
        ('\'', '\''),
        ('"', '"'),
        ('`', '`'),
        ('\u{2018}', '\u{2019}'),
    ];
    for (open, close) in pairs {
        if let Some(inner) = s
            .strip_prefix(open)
            .and_then(|rest| rest.strip_suffix(close))
        {
            return inner;
        }
    }
    s
}

fn looks_like_id(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Translate one supersede event into a `Misled` signal on each
/// predecessor id the edit named.
#[must_use]
pub fn signals_from_supersede(successor: &str, title: &str, content: &str) -> Vec<Signal> {
    superseded_predecessors(title, content)
        .into_iter()
        .map(|predecessor| Signal {
            memory_id: predecessor,
            outcome: Outcome::Misled,
            cause: SignalCause::Superseded {
                successor: successor.to_string(),
            },
        })
        .collect()
}

/// Walk a sequence of refinement records and emit every retrieval
/// signal the deterministic classes yield. `memory_targeting` snapshots
/// each memory's current `targetsFingerprints` (for the RAVO case) and
/// is read once; pass an owned `Vec` or clone the caller's view. The
/// caller composes the record iterator over `refinement_history.jsonl`
/// and the targeting snapshot over `harness_state.json`.
pub fn extract_signals<I>(records: I, memory_targeting: &[(String, Vec<String>)]) -> Vec<Signal>
where
    I: IntoIterator<Item = RefinementRecord>,
{
    let mut signals = Vec::new();
    for rec in records {
        if rec.rejection_cause.as_deref() == Some("gate") {
            let missed = failure_fingerprints_from_missed(&rec.missed_criteria);
            signals.extend(signals_from_ravo_rejection(
                missed,
                memory_targeting
                    .iter()
                    .map(|(m, f)| (m.as_str(), f.clone())),
            ));
        }
        for edit in &rec.applied_edits {
            signals.extend(signals_from_supersede(&edit.id, &edit.title, &edit.content));
        }
    }
    signals
}

/// The narrow view of a cert record this module needs. pa-ledger builds
/// these from the full `RefinementResult` records it already reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefinementRecord {
    /// `ravo.decision == "reject_*"` → fill from `ravo.missedCriteria`;
    /// absent otherwise.
    pub missed_criteria: Vec<String>,
    /// The record's `rejectionCause` (`"gate"` / `"screen"` /
    /// `"judge_unavailable"` / `None`).
    pub rejection_cause: Option<String>,
    /// The applied edits (empty on a rejection).
    pub applied_edits: Vec<AppliedEditView>,
}

/// Minimum applied-edit shape the signal extractor needs: the new entry's
/// id plus its title and content prose (to scan for a supersede event).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedEditView {
    /// The id of the memory the edit created or updated.
    pub id: String,
    /// The entry's title prose.
    pub title: String,
    /// The entry's content prose.
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_prefixes_strip() {
        assert_eq!(
            failure_fingerprints_from_missed(&[
                "failure:ca578aff209c6a3c".into(),
                "scope".into(),
                "failure:deadbeef".into(),
            ]),
            vec!["ca578aff209c6a3c".to_string(), "deadbeef".into()]
        );
    }

    #[test]
    fn ravo_signals_fire_for_memories_targeting_missed_fingerprints() {
        let missed = ["ca578aff209c6a3c".to_string()];
        let memories = [
            (
                "kernel_inspect_getsource_cell_function_unsafe".to_string(),
                vec![
                    "66721a3acd3f68dc".to_string(),
                    "748dbd7f530d0aa6".to_string(),
                ],
            ),
            (
                "ers_rs_identity_fallback".to_string(),
                vec!["ca578aff209c6a3c".to_string()],
            ),
        ];
        let sigs = signals_from_ravo_rejection(missed, memories);
        assert_eq!(sigs.len(), 1);
        assert_eq!(sigs[0].memory_id, "ers_rs_identity_fallback");
        assert_eq!(sigs[0].outcome, Outcome::Helped);
        assert_eq!(
            sigs[0].cause,
            SignalCause::RavoCoverage {
                fingerprint: "ca578aff209c6a3c".into()
            }
        );
    }

    #[test]
    fn supersede_extracts_predecessor_ids() {
        assert_eq!(
            superseded_predecessors("ers-rs: PR #58 landed; supersedes ers_sf_not_needed", "",),
            vec!["ers_sf_not_needed".to_string()]
        );
        assert_eq!(
            superseded_predecessors(
                "",
                "This supersedes the kernel_list_old lesson; see the new ranges.",
            ),
            vec!["kernel_list_old".to_string()]
        );
        assert_eq!(
            superseded_predecessors("", "supersedes 'memory_with_quotes'."),
            vec!["memory_with_quotes".to_string()]
        );
    }

    #[test]
    fn supersede_without_id_token_is_ignored() {
        // The cert memory flagged this case: refiners sometimes supersede
        // by prose rather than id ("the 'not needed' decision"). Returns
        // empty here; a classifier in pa-ledger would resolve it.
        assert!(superseded_predecessors("", "supersedes the 'not needed' decision").is_empty());
    }

    #[test]
    fn extract_signals_combines_ravo_and_supersede_records() {
        let records = vec![
            RefinementRecord {
                missed_criteria: vec!["failure:deadbeef".into()],
                rejection_cause: Some("gate".into()),
                applied_edits: Vec::new(),
            },
            RefinementRecord {
                missed_criteria: Vec::new(),
                rejection_cause: None,
                applied_edits: vec![AppliedEditView {
                    id: "memory_new".into(),
                    title: "fresh take; supersedes memory_old".into(),
                    content: String::new(),
                }],
            },
        ];
        let memories = vec![
            (
                "memory_covers".to_string(),
                vec!["deadbeef".to_string(), "deadc0de".to_string()],
            ),
            ("memory_elsewhere".to_string(), vec!["cafef00d".to_string()]),
        ];
        let sigs = extract_signals(records, &memories);
        assert_eq!(sigs.len(), 2);
        assert!(
            sigs.iter()
                .any(|s| s.memory_id == "memory_covers" && s.outcome == Outcome::Helped)
        );
        assert!(
            sigs.iter()
                .any(|s| s.memory_id == "memory_old" && s.outcome == Outcome::Misled)
        );
    }
}
