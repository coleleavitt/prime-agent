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

use std::collections::{BTreeSet, HashMap};

use super::planner::{RefinementProposal, rollback_proposal};
use super::{HarnessEntry, HarnessState, RefinementKind, RefinementResult};

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

/// One revert the caller can apply: the rollback-shape proposal (from
/// [`rollback_proposal`]), the originating refinement result's id, and the
/// hits that motivated it (so the recorded `reason` text can cite them).
#[derive(Debug, Clone, PartialEq)]
pub struct RecurrenceRevertPlan {
    pub refinement_result_id: String,
    pub proposal: RefinementProposal,
    pub motivating_hits: Vec<RecurrenceHit>,
}

/// Plan the reverts: for each `RecurrenceHit`, find the latest
/// [`RefinementResult`] in `history` whose applied edits created or
/// updated the recurred entry, then build one rollback proposal per such
/// result (groups hits that trace to the same result so reverts stay
/// coherent). A hit whose entry is not in the history is skipped — a
/// recurrence of an entry that was never refined has nothing to revert.
#[must_use]
pub fn plan_recurrence_reverts(
    hits: &[RecurrenceHit],
    history: &[RefinementResult],
) -> Vec<RecurrenceRevertPlan> {
    // Group hits by the refinement result that produced the entry (if
    // any). An entry may appear in several results; pick the newest that
    // touched it — that is the one whose edits the rollback must undo.
    let mut by_result: HashMap<String, Vec<RecurrenceHit>> = HashMap::new();
    for hit in hits {
        let Some(result) = latest_result_for_entry(&hit.entry_id, hit.entry_kind, history) else {
            continue;
        };
        by_result
            .entry(result.id.clone())
            .or_default()
            .push(hit.clone());
    }
    // Stable output: sort by result id, then hits by (entry_id,
    // fingerprint_id), so two callers with the same inputs see the same
    // plan (tests benefit; downstream logging too).
    let mut plans: Vec<RecurrenceRevertPlan> = by_result
        .into_iter()
        .filter_map(|(result_id, mut motivating_hits)| {
            motivating_hits.sort_by(|a, b| {
                (&a.entry_id, &a.fingerprint_id).cmp(&(&b.entry_id, &b.fingerprint_id))
            });
            let result = history.iter().find(|item| item.id == result_id)?;
            let mut proposal = rollback_proposal(result);
            proposal.rationale = recurrence_rationale(&motivating_hits, &result_id);
            Some(RecurrenceRevertPlan {
                refinement_result_id: result_id,
                proposal,
                motivating_hits,
            })
        })
        .collect();
    plans.sort_by(|a, b| a.refinement_result_id.cmp(&b.refinement_result_id));
    plans
}

fn latest_result_for_entry<'a>(
    entry_id: &str,
    kind: RefinementKind,
    history: &'a [RefinementResult],
) -> Option<&'a RefinementResult> {
    history.iter().rev().find(|result| {
        result
            .applied_edits
            .iter()
            .any(|edit| edit.applied && edit.kind == kind && edit.id == entry_id)
    })
}

fn recurrence_rationale(hits: &[RecurrenceHit], result_id: &str) -> String {
    let distinct_fingerprints: BTreeSet<&str> =
        hits.iter().map(|hit| hit.fingerprint_id.as_str()).collect();
    format!(
        "Recurrence revert: refinement {result_id} targeted {} fingerprint(s) that have reoccurred in the failure ledger ({}).",
        distinct_fingerprints.len(),
        distinct_fingerprints
            .into_iter()
            .take(5)
            .collect::<Vec<_>>()
            .join(", "),
    )
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
            lane: crate::refinement::RefinementLane::Lesson,
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

    fn applied_edit(
        kind: RefinementKind,
        id: &str,
        before: Option<HarnessEntry>,
        after: Option<HarnessEntry>,
    ) -> crate::refinement::AppliedRefinementEdit {
        crate::refinement::AppliedRefinementEdit {
            action: if before.is_some() {
                crate::refinement::RefinementAction::Update
            } else {
                crate::refinement::RefinementAction::Create
            },
            kind,
            id: id.to_string(),
            title: after.as_ref().map(|entry| entry.title.clone()),
            content: after.as_ref().map(|entry| entry.content.clone()),
            path: after.as_ref().map(|entry| entry.path.clone()),
            reference: after.as_ref().map(|entry| entry.reference.clone()),
            arguments: after.as_ref().map(|entry| entry.arguments.clone()),
            metadata: after.as_ref().map(|entry| entry.metadata.clone()),
            before,
            after,
            applied: true,
            error: None,
            reason: None,
        }
    }

    fn refinement_result(
        id: &str,
        applied: Vec<crate::refinement::AppliedRefinementEdit>,
    ) -> RefinementResult {
        RefinementResult {
            id: id.to_string(),
            summary: id.to_string(),
            rationale: String::new(),
            expected_outcome: String::new(),
            applied_edits: applied,
            harness_state_path: String::new(),
            rollback_of: None,
            scope: None,
            extensions: serde_json::Map::new(),
        }
    }

    #[test]
    fn plan_produces_one_rollback_per_originating_result_grouping_hits() {
        let entry_a = entry("e1", "2026-11-04T00:00:00Z", &["fp-a"]);
        let entry_b = entry("e2", "2026-11-04T00:00:00Z", &["fp-b"]);
        let history = vec![
            refinement_result(
                "r1",
                vec![applied_edit(
                    RefinementKind::Memory,
                    "e1",
                    None,
                    Some(entry_a.clone()),
                )],
            ),
            refinement_result(
                "r2",
                vec![applied_edit(
                    RefinementKind::Memory,
                    "e2",
                    None,
                    Some(entry_b.clone()),
                )],
            ),
        ];
        let s = state(vec![entry_a, entry_b]);
        let owned = observations(&[
            ("fp-a", "2026-11-05T00:00:00Z", 2),
            ("fp-b", "2026-11-05T01:00:00Z", 3),
        ]);
        let hits = probe_recurrence(&s, &as_ref_map(&owned));
        let plans = plan_recurrence_reverts(&hits, &history);
        assert_eq!(plans.len(), 2);
        let plan_ids: Vec<&str> = plans
            .iter()
            .map(|plan| plan.refinement_result_id.as_str())
            .collect();
        assert_eq!(plan_ids, vec!["r1", "r2"]);
        assert!(
            plans[0]
                .proposal
                .rationale
                .contains("Recurrence revert: refinement r1")
        );
        // Each plan's rollback proposal reverts the edit the originating
        // result applied (a `create` becomes a `delete`).
        assert_eq!(plans[0].proposal.edits.len(), 1);
        assert!(matches!(
            plans[0].proposal.edits[0].action,
            Some(crate::refinement::RefinementAction::Delete)
        ));
    }

    #[test]
    fn plan_skips_a_hit_whose_entry_no_refinement_result_touched() {
        // The entry has a recurrence but no result in the history ever
        // created or updated it (foreign origin: host-written, or history
        // got pruned). The caller sees an empty plan for that hit.
        let orphan = entry("orphan", "2026-11-04T00:00:00Z", &["fp-a"]);
        let s = state(vec![orphan]);
        let owned = observations(&[("fp-a", "2026-11-05T00:00:00Z", 1)]);
        let hits = probe_recurrence(&s, &as_ref_map(&owned));
        assert_eq!(hits.len(), 1);
        let plans = plan_recurrence_reverts(&hits, &[]);
        assert!(plans.is_empty());
    }

    #[test]
    fn plan_picks_the_latest_result_that_touched_the_entry() {
        let entry_a = entry("e1", "2026-11-04T00:00:00Z", &["fp-a"]);
        let history = vec![
            refinement_result(
                "r1",
                vec![applied_edit(
                    RefinementKind::Memory,
                    "e1",
                    None,
                    Some(entry_a.clone()),
                )],
            ),
            refinement_result(
                "r2",
                vec![applied_edit(
                    RefinementKind::Memory,
                    "e1",
                    Some(entry_a.clone()),
                    Some(entry_a.clone()),
                )],
            ),
        ];
        let s = state(vec![entry_a]);
        let owned = observations(&[("fp-a", "2026-11-05T00:00:00Z", 1)]);
        let hits = probe_recurrence(&s, &as_ref_map(&owned));
        let plans = plan_recurrence_reverts(&hits, &history);
        // Only one plan: the newer `r2` wins, never both.
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].refinement_result_id, "r2");
    }
}
