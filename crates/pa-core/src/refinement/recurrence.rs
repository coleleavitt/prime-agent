//! Post-fix recurrence probe (design doc §2's yellow measurement box).
//!
//! Every accepted refinement edit can carry a list of failure-ledger
//! fingerprints it was meant to prevent recurrence of
//! ([`super::HarnessEntry::targets_fingerprints`]). The probe answers one
//! question: for each enabled entry with targets, has any targeted
//! fingerprint been observed in the ledger *after* the entry's last
//! `updated_at`?
//!
//! The probe is pure read-only: it takes a snapshot of the harness state
//! and a snapshot of the ledger and returns the recurrences it sees. The
//! caller (`auto_refine_trigger` in the next commit) then decides what to
//! do with each hit — revert the entry, flag it for a stronger fix, or
//! surface it to the operator.

use std::collections::HashMap;

use super::{HarnessEntry, HarnessState, RefinementKind};

/// One entry's fingerprint recurred after the entry landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurrenceHit {
    pub entry_id: String,
    pub entry_kind: RefinementKind,
    pub entry_updated_at: String,
    pub fingerprint_id: String,
    /// The ledger's recorded `last_seen_at` for the fingerprint (ISO).
    pub last_seen_at: String,
    /// The ledger's `count` for the fingerprint at probe time. Not scoped
    /// to "since the entry's `updated_at`" (the ledger does not keep
    /// per-interval counts); the caller uses it as a magnitude signal.
    pub total_count: u64,
}

/// Each `(fingerprint_id → (last_seen_at, count))`. The signature keeps the
/// probe independent of `pa-ledger` (which depends on `pa-core`, so the
/// crate graph forbids the reverse import): a caller that holds a
/// `pa_ledger::ledger::FailureLedger` builds the map with a one-liner,
/// `ledger.failures.iter().map(|(id, rec)| (id.as_str(), FingerprintState {
/// last_seen_at: rec.last_seen_at.clone(), count: rec.count })).collect()`,
/// and the probe stays cheap and stateless.
pub type FingerprintObservations<'a> = HashMap<&'a str, FingerprintState>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FingerprintState {
    pub last_seen_at: String,
    pub count: u64,
}

/// Walk `state`'s entries and return every `(entry, fingerprint)` pair whose
/// ledger observation is strictly later than the entry's `updated_at`.
///
/// The compare is ASCII-lexicographic on ISO-8601 timestamps — correct for
/// `YYYY-MM-DDTHH:MM:SSZ`-style strings the rest of the harness uses, and
/// does not depend on a date parser.
#[must_use]
pub fn probe_recurrence(
    state: &HarnessState,
    observations: &FingerprintObservations<'_>,
) -> Vec<RecurrenceHit> {
    let mut hits = Vec::new();
    for (kind, entries) in &state.entries {
        for entry in entries.values() {
            collect_hits(*kind, entry, observations, &mut hits);
        }
    }
    hits
}

fn collect_hits(
    kind: RefinementKind,
    entry: &HarnessEntry,
    observations: &FingerprintObservations<'_>,
    out: &mut Vec<RecurrenceHit>,
) {
    for fingerprint in &entry.targets_fingerprints {
        let Some(state) = observations.get(fingerprint.as_str()) else {
            continue;
        };
        if state.last_seen_at.as_str() <= entry.updated_at.as_str() {
            continue;
        }
        out.push(RecurrenceHit {
            entry_id: entry.id.clone(),
            entry_kind: kind,
            entry_updated_at: entry.updated_at.clone(),
            fingerprint_id: fingerprint.clone(),
            last_seen_at: state.last_seen_at.clone(),
            total_count: state.count,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::refinement::HarnessScope;

    fn entry(id: &str, updated_at: &str, targets: &[&str]) -> HarnessEntry {
        HarnessEntry {
            id: id.to_string(),
            kind: RefinementKind::Memory,
            title: id.to_string(),
            content: String::new(),
            path: "general".to_string(),
            scope: Some(HarnessScope::Local),
            reference: serde_json::Map::new(),
            arguments: serde_json::Map::new(),
            metadata: serde_json::Map::new(),
            source: "refine".to_string(),
            created_at: updated_at.to_string(),
            updated_at: updated_at.to_string(),
            version: 1,
            targets_fingerprints: targets.iter().map(|target| (*target).to_string()).collect(),
            extensions: serde_json::Map::new(),
        }
    }

    fn state(entries: Vec<HarnessEntry>) -> HarnessState {
        let mut by_kind: BTreeMap<RefinementKind, BTreeMap<String, HarnessEntry>> = BTreeMap::new();
        for entry in entries {
            by_kind
                .entry(entry.kind)
                .or_default()
                .insert(entry.id.clone(), entry);
        }
        HarnessState {
            schema: 1,
            entries: by_kind,
            refinements: Vec::new(),
            extensions: serde_json::Map::new(),
        }
    }

    fn observations(entries: &[(&str, &str, u64)]) -> HashMap<String, FingerprintState> {
        entries
            .iter()
            .map(|(id, last_seen_at, count)| {
                (
                    (*id).to_string(),
                    FingerprintState {
                        last_seen_at: (*last_seen_at).to_string(),
                        count: *count,
                    },
                )
            })
            .collect()
    }

    fn as_ref_map(owned: &HashMap<String, FingerprintState>) -> FingerprintObservations<'_> {
        owned
            .iter()
            .map(|(id, state)| (id.as_str(), state.clone()))
            .collect()
    }

    #[test]
    fn a_recurrence_after_the_entry_is_reported() {
        let s = state(vec![entry("e1", "2026-11-04T00:00:00Z", &["fp-a", "fp-b"])]);
        let owned = observations(&[
            ("fp-a", "2026-11-05T12:00:00Z", 3),
            ("fp-b", "2026-11-03T01:00:00Z", 1), // before the entry → no hit
        ]);
        let hits = probe_recurrence(&s, &as_ref_map(&owned));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry_id, "e1");
        assert_eq!(hits[0].fingerprint_id, "fp-a");
        assert_eq!(hits[0].total_count, 3);
    }

    #[test]
    fn an_entry_with_no_targets_is_skipped() {
        let s = state(vec![entry("e1", "2026-11-04T00:00:00Z", &[])]);
        let owned = observations(&[("fp-a", "2026-11-05T12:00:00Z", 3)]);
        assert!(probe_recurrence(&s, &as_ref_map(&owned)).is_empty());
    }

    #[test]
    fn a_fingerprint_not_in_the_ledger_is_skipped() {
        let s = state(vec![entry("e1", "2026-11-04T00:00:00Z", &["fp-missing"])]);
        let owned = observations(&[("fp-other", "2026-11-05T12:00:00Z", 3)]);
        assert!(probe_recurrence(&s, &as_ref_map(&owned)).is_empty());
    }

    #[test]
    fn two_entries_targeting_the_same_fingerprint_both_report() {
        let s = state(vec![
            entry("e1", "2026-11-04T00:00:00Z", &["fp-a"]),
            entry("e2", "2026-11-04T01:00:00Z", &["fp-a"]),
        ]);
        let owned = observations(&[("fp-a", "2026-11-05T00:00:00Z", 2)]);
        let hits = probe_recurrence(&s, &as_ref_map(&owned));
        let ids: Vec<&str> = hits.iter().map(|hit| hit.entry_id.as_str()).collect();
        assert_eq!(ids, vec!["e1", "e2"]);
    }

    #[test]
    fn an_observation_at_exactly_updated_at_is_not_a_recurrence() {
        let s = state(vec![entry("e1", "2026-11-04T00:00:00Z", &["fp-a"])]);
        let owned = observations(&[("fp-a", "2026-11-04T00:00:00Z", 1)]);
        assert!(probe_recurrence(&s, &as_ref_map(&owned)).is_empty());
    }
}
