//! Post-commit referee replays: the only way a committed refinement loses
//! trust (TS `refinement/trust-adjudication.ts`).
//!
//! A window's claimed failure recurring is not by itself evidence against
//! any entry the commit wrote. A replay is planned only for a skill entry
//! the window touched whose imports are still the ones the commit
//! recorded, only on the newest overlapping window that wrote them, and
//! only when the recurrence's own derived case probes one of those imports.
//! It runs the same verified probes, in the same `skill-import`
//! environment, as the gate's referee.

use std::sync::atomic::{AtomicBool, Ordering};

use indexmap::IndexMap;
use pa_core::refinement::HarnessScope;
use pa_ledger::{
    apply_replay_verifications, replay_probe_of, FailureLedger, FailureRecord, ReplayCase,
    ReplayVerification,
};
use tracing::Instrument;

use crate::js::locale_compare;
use crate::referee::{
    adjudicate_failure_claims, replay_applies_to_skill_imports, RefereeVerdictStatus, ReplayRunner,
};
use crate::trust::{
    current_skill_imports, parse_harness_entry_ref, same_modules, TrustAdjudicationStatus,
    TrustEntries, TrustOutcome, TrustWindow, TrustWindowEvidence, TrustWindows,
    MAX_TRUST_ADJUDICATION_RUNS,
};

/// Replays planned (and awaiting) per batch.
pub const MAX_TRUST_ADJUDICATION_JOBS: usize = 8;

/// A claimed fingerprint recurring inside an open window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRecurrence {
    pub proposal_id: String,
    pub fingerprint_id: String,
    pub ordinal: u64,
    /// Valid probes derived from this batch's actionable occurrences of the
    /// fingerprint, distinct by source.
    pub observed_cases: Vec<ReplayCase>,
}

/// One planned replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustAdjudicationJob {
    pub scope: HarnessScope,
    pub proposal_id: String,
    /// The touched `skill:<id>` the replay runs for.
    pub entry: String,
    pub fingerprint_id: String,
    pub ordinal: u64,
    /// The imports the window recorded for the entry (also its current ones).
    pub skill_imports: Vec<String>,
    pub record: FailureRecord,
}

impl TrustAdjudicationJob {
    /// `scope proposal entry fingerprint`: a job's identity across batches.
    #[must_use]
    pub fn key(&self) -> String {
        let scope = match self.scope {
            HarnessScope::Local => "local",
            HarnessScope::Global => "global",
        };
        format!(
            "{scope} {} {} {}",
            self.proposal_id, self.entry, self.fingerprint_id
        )
    }
}

/// A job whose observed case has not reproduced yet; released when the
/// self-check verifies one of `sources`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitingTrustAdjudication {
    pub job: TrustAdjudicationJob,
    pub sources: Vec<String>,
}

/// A replay verdict, scoped to the store whose window it speaks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedTrustEvidence {
    pub scope: HarnessScope,
    pub evidence: TrustWindowEvidence,
}

fn in_range(window: &TrustWindow, ordinal: u64) -> bool {
    ordinal >= window.committed_turn && ordinal <= window.until_turn
}

/// The open windows `ordinal` falls in that claimed a fingerprint which
/// recurred in this batch (`recurred`: fingerprint -> the batch's derived
/// cases), sorted by proposal then fingerprint. Only valid probes are
/// carried as observed cases.
#[must_use]
pub fn find_trust_window_recurrences(
    windows: Option<&TrustWindows>,
    recurred: &IndexMap<String, Vec<ReplayCase>>,
    ordinal: u64,
) -> Vec<TrustRecurrence> {
    let mut recurrences = Vec::new();
    for window in windows.into_iter().flat_map(IndexMap::values) {
        if window.outcome != TrustOutcome::Open || !in_range(window, ordinal) {
            continue;
        }
        for fingerprint_id in &window.claimed_fingerprints {
            let Some(cases) = recurred.get(fingerprint_id) else {
                continue;
            };
            let mut by_source: Vec<ReplayCase> = Vec::new();
            for case in cases {
                if replay_probe_of(&case.source).is_some()
                    && !by_source.iter().any(|kept| kept.source == case.source)
                {
                    by_source.push(case.clone());
                }
            }
            recurrences.push(TrustRecurrence {
                proposal_id: window.proposal_id.clone(),
                fingerprint_id: fingerprint_id.clone(),
                ordinal,
                observed_cases: by_source,
            });
        }
    }
    recurrences.sort_by(|left, right| {
        locale_compare(&left.proposal_id, &right.proposal_id)
            .then_with(|| locale_compare(&left.fingerprint_id, &right.fingerprint_id))
    });
    recurrences
}

/// The window a replay fact about (`entry`, `fingerprint_id`) at `ordinal`
/// is charged to: the newest by commit ordinal (ties: proposal id,
/// descending) of the windows that claimed the fingerprint, touched the
/// entry with the same recorded imports, and hold the ordinal.
fn newest_attributable_window<'a>(
    windows: &'a TrustWindows,
    entry: &str,
    fingerprint_id: &str,
    imports: &[String],
    ordinal: u64,
) -> Option<&'a TrustWindow> {
    let mut newest: Option<&TrustWindow> = None;
    for window in windows.values() {
        if window.outcome == TrustOutcome::Unmeasured || !in_range(window, ordinal) {
            continue;
        }
        if !window
            .claimed_fingerprints
            .iter()
            .any(|claimed| claimed == fingerprint_id)
            || !window.touched.iter().any(|touched| touched == entry)
        {
            continue;
        }
        let Some(recorded) = window
            .skill_imports
            .as_ref()
            .and_then(|imports| imports.get(entry))
        else {
            continue;
        };
        if !same_modules(recorded, imports) {
            continue;
        }
        let newer = newest.is_none_or(|newest| {
            window.committed_turn > newest.committed_turn
                || (window.committed_turn == newest.committed_turn
                    && locale_compare(&window.proposal_id, &newest.proposal_id).is_gt())
        });
        if newer {
            newest = Some(window);
        }
    }
    newest
}

/// The input of [`plan_trust_adjudications`].
pub struct TrustPlanInput<'a> {
    pub scope: HarnessScope,
    pub windows: Option<&'a TrustWindows>,
    pub recurrences: &'a [TrustRecurrence],
    pub entries: &'a dyn TrustEntries,
    pub record_of: &'a dyn Fn(&str) -> Option<FailureRecord>,
}

/// Plan the replays a batch of recurrences warrants in one scope: a job
/// for a (window, touched skill, fingerprint) only when the skill still
/// imports what the window recorded, the window is the newest that wrote
/// those imports over the ordinal, it is not faulted, has no upheld verdict
/// and fewer than [`MAX_TRUST_ADJUDICATION_RUNS`] runs for the pair, and a
/// case this batch observed probes one of the imports. It is queued when
/// the record already holds a verified case probing them, and otherwise
/// awaits the self-check of the observed sources. Each list is capped at
/// [`MAX_TRUST_ADJUDICATION_JOBS`]; answers (jobs, awaiting).
#[must_use]
pub fn plan_trust_adjudications(
    input: &TrustPlanInput<'_>,
) -> (Vec<TrustAdjudicationJob>, Vec<AwaitingTrustAdjudication>) {
    let mut jobs = Vec::new();
    let mut awaiting = Vec::new();
    let Some(windows) = input.windows else {
        return (jobs, awaiting);
    };
    let mut planned: Vec<String> = Vec::new();
    for recurrence in input.recurrences {
        let Some(window) = windows.get(&recurrence.proposal_id) else {
            continue;
        };
        let fingerprint_id = &recurrence.fingerprint_id;
        let ordinal = recurrence.ordinal;
        if matches!(
            window.outcome,
            TrustOutcome::Faulted | TrustOutcome::Unmeasured
        ) {
            continue;
        }
        if !window.claimed_fingerprints.contains(fingerprint_id) || !in_range(window, ordinal) {
            continue;
        }
        for entry in &window.touched {
            if parse_harness_entry_ref(entry).is_none_or(|(kind, _)| kind != "skill") {
                continue;
            }
            let Some(recorded) = window
                .skill_imports
                .as_ref()
                .and_then(|imports| imports.get(entry))
                .filter(|recorded| !recorded.is_empty())
            else {
                continue;
            };
            let Some(current) = current_skill_imports(input.entries, entry) else {
                continue;
            };
            if !same_modules(&current, recorded) {
                continue;
            }
            let newest =
                newest_attributable_window(windows, entry, fingerprint_id, recorded, ordinal);
            if newest.is_none_or(|newest| newest.proposal_id != window.proposal_id) {
                continue;
            }
            let adjudication = window
                .adjudications
                .iter()
                .flatten()
                .find(|item| &item.entry == entry && &item.fingerprint_id == fingerprint_id);
            if adjudication.is_some_and(|item| {
                item.status == TrustAdjudicationStatus::Upheld
                    || item.runs.len() >= MAX_TRUST_ADJUDICATION_RUNS
            }) {
                continue;
            }
            let mut sources: Vec<String> = Vec::new();
            for case in &recurrence.observed_cases {
                if replay_applies_to_skill_imports(case, recorded)
                    && !sources.contains(&case.source)
                {
                    sources.push(case.source.clone());
                }
            }
            // TS `.sort()`: UTF-16 code unit order.
            sources.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
            if sources.is_empty() {
                continue;
            }
            let Some(record) = (input.record_of)(fingerprint_id) else {
                continue;
            };
            let job = TrustAdjudicationJob {
                scope: input.scope,
                proposal_id: window.proposal_id.clone(),
                entry: entry.clone(),
                fingerprint_id: fingerprint_id.clone(),
                ordinal,
                skill_imports: recorded.clone(),
                record,
            };
            let key = job.key();
            if planned.contains(&key) {
                continue;
            }
            planned.push(key);
            let verified = job
                .record
                .verified_replay_cases()
                .into_iter()
                .any(|case| replay_applies_to_skill_imports(case, recorded));
            if verified {
                if jobs.len() < MAX_TRUST_ADJUDICATION_JOBS {
                    jobs.push(job);
                }
            } else if awaiting.len() < MAX_TRUST_ADJUDICATION_JOBS {
                awaiting.push(AwaitingTrustAdjudication { job, sources });
            }
        }
    }
    (jobs, awaiting)
}

/// What a finished self-check batch does to an awaiting job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedAdjudication {
    /// A verification of the batch verified one of the awaited sources.
    pub matched: bool,
    /// The job, its record carrying the verifications, when a verified
    /// case now probes its imports.
    pub job: Option<TrustAdjudicationJob>,
}

/// Release an awaiting job against the verifications a self-check landed.
#[must_use]
pub fn release_awaiting_trust_adjudication(
    awaiting: &AwaitingTrustAdjudication,
    verifications: &[ReplayVerification],
) -> ReleasedAdjudication {
    let job = &awaiting.job;
    let matches: Vec<ReplayVerification> = verifications
        .iter()
        .filter(|verification| {
            verification.fingerprint_id == job.fingerprint_id
                && awaiting.sources.contains(&verification.source)
        })
        .cloned()
        .collect();
    if matches.is_empty() {
        return ReleasedAdjudication {
            matched: false,
            job: None,
        };
    }
    let mut single = FailureLedger::default();
    single
        .failures
        .insert(job.fingerprint_id.clone(), job.record.clone());
    let record = apply_replay_verifications(&single, &matches)
        .failures
        .shift_remove(&job.fingerprint_id);
    let job = record
        .filter(|record| {
            record
                .verified_replay_cases()
                .into_iter()
                .any(|case| replay_applies_to_skill_imports(case, &job.skill_imports))
        })
        .map(|record| TrustAdjudicationJob {
            record,
            ..job.clone()
        });
    ReleasedAdjudication { matched: true, job }
}

/// The input of [`adjudicate_trust_recurrences`].
pub struct TrustAdjudicationRun<'a> {
    pub runner: &'a dyn ReplayRunner,
    /// Extra `sys.path` roots (the toolforge source roots).
    pub sys_path: &'a [String],
    pub session_id: &'a str,
    /// Set to abandon the batch: a job aborted before or during its run
    /// yields nothing.
    pub aborted: &'a AtomicBool,
    pub now: &'a dyn Fn() -> String,
}

/// Run a batch of planned replays serially under one
/// `harness.trust.adjudicate` span (a root: replays outlive the turn that
/// observed the recurrence) and return the verdicts as evidence scoped to
/// the window and entry each ran for. A verdict that is not a replay
/// result (`no_evidence`, `not_applicable`) yields nothing and consumes no
/// run.
pub async fn adjudicate_trust_recurrences(
    jobs: &[TrustAdjudicationJob],
    run: TrustAdjudicationRun<'_>,
) -> Vec<ScopedTrustEvidence> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let mut windows: Vec<String> = jobs
        .iter()
        .map(|job| format!("{:?} {}", job.scope, job.proposal_id))
        .collect();
    windows.sort();
    windows.dedup();
    let span = tracing::info_span!(
        parent: None,
        "harness.trust.adjudicate",
        session.id = run.session_id,
        trust.jobs = jobs.len(),
        trust.windows = windows.len(),
        trust.ran = tracing::field::Empty,
        trust.upheld = tracing::field::Empty,
        trust.cleared = tracing::field::Empty,
        trust.unverifiable = tracing::field::Empty,
        trust.skipped = tracing::field::Empty,
        trust.aborted = tracing::field::Empty,
    );
    let work = async {
        let mut evidence = Vec::new();
        let (mut ran, mut upheld, mut cleared, mut unverifiable, mut skipped) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut aborted = false;
        for job in jobs {
            if run.aborted.load(Ordering::SeqCst) {
                aborted = true;
                break;
            }
            let verdicts = adjudicate_failure_claims(
                std::slice::from_ref(&job.record),
                std::slice::from_ref(&job.fingerprint_id),
                &job.skill_imports,
                run.sys_path,
                run.runner,
            )
            .await;
            if run.aborted.load(Ordering::SeqCst) {
                aborted = true;
                break;
            }
            ran += 1;
            let status = verdicts
                .iter()
                .find(|verdict| verdict.fingerprint_id == job.fingerprint_id)
                .map(|verdict| verdict.status);
            let status = match status {
                Some(RefereeVerdictStatus::Upheld) => {
                    upheld += 1;
                    TrustAdjudicationStatus::Upheld
                }
                Some(RefereeVerdictStatus::Cleared) => {
                    cleared += 1;
                    TrustAdjudicationStatus::Cleared
                }
                Some(RefereeVerdictStatus::Unverifiable) => {
                    unverifiable += 1;
                    TrustAdjudicationStatus::Unverifiable
                }
                _ => {
                    skipped += 1;
                    continue;
                }
            };
            evidence.push(ScopedTrustEvidence {
                scope: job.scope,
                evidence: TrustWindowEvidence::Adjudication {
                    proposal_id: job.proposal_id.clone(),
                    entry: job.entry.clone(),
                    fingerprint_id: job.fingerprint_id.clone(),
                    status,
                    ordinal: job.ordinal,
                    at: (run.now)(),
                },
            });
        }
        (
            evidence,
            [ran, upheld, cleared, unverifiable, skipped],
            aborted,
        )
    };
    let (evidence, [ran, upheld, cleared, unverifiable, skipped], aborted) =
        work.instrument(span.clone()).await;
    span.record("trust.ran", ran);
    span.record("trust.upheld", upheld);
    span.record("trust.cleared", cleared);
    span.record("trust.unverifiable", unverifiable);
    span.record("trust.skipped", skipped);
    span.record("trust.aborted", aborted);
    evidence
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    use pa_core::refinement::{empty_harness_state, HarnessState, RefinementKind};
    use pa_ledger::FailureFingerprint;
    use serde_json::{json, Value};

    use super::*;
    use crate::referee::{ReplayEnvironment, ReplayOutcome};
    use crate::trust::normalize_trust_windows;

    const FP: &str = "fp_missing_module";
    const OTHER_FP: &str = "fp_other";
    const VERIFIED_AT: &str = "2026-09-16T08:00:00.000Z";

    fn probe(source: &str, verified: bool) -> ReplayCase {
        ReplayCase {
            language: "python".to_string(),
            source: source.to_string(),
            exception_class: Some("ModuleNotFoundError".to_string()),
            sys_path: None,
            verified_at: verified.then(|| VERIFIED_AT.to_string()),
        }
    }

    fn failure_record(cases: Vec<ReplayCase>) -> FailureRecord {
        FailureRecord {
            fingerprint: FailureFingerprint {
                id: FP.to_string(),
                ..pa_ledger::fingerprint_failure(
                    pa_ledger::FailureKind::PythonException,
                    Some("ipython"),
                    Some("ModuleNotFoundError"),
                    "no module named ?",
                )
            },
            count: 3,
            first_seen_turn: 1,
            last_seen_turn: 3,
            first_seen_at: VERIFIED_AT.to_string(),
            last_seen_at: VERIFIED_AT.to_string(),
            excerpt: "ModuleNotFoundError: No module named 'pkg'".to_string(),
            addressed_by_proposal_ids: Vec::new(),
            replay_cases: cases,
            non_actionable_count: None,
        }
    }

    fn entries(skills: &[(&str, &str)]) -> HarnessState {
        let mut state = empty_harness_state();
        let entry = |kind: &str, id: &str, reference: Value| {
            serde_json::from_value(json!({
                "id": id, "kind": kind, "title": id, "content": id, "path": "general",
                "scope": "local", "reference": reference, "arguments": {}, "metadata": {},
                "source": "refine", "created_at": VERIFIED_AT, "updated_at": VERIFIED_AT,
                "version": 1
            }))
            .unwrap()
        };
        for (id, module) in skills {
            state
                .entries
                .get_mut(&RefinementKind::Skill)
                .unwrap()
                .insert(
                    (*id).to_string(),
                    entry(
                        "skill",
                        id,
                        json!({"type": "python", "import": module, "callable": "run"}),
                    ),
                );
        }
        for (kind, key, id) in [
            ("memory", RefinementKind::Memory, "m"),
            ("prompt", RefinementKind::Prompt, "p"),
            ("subagent", RefinementKind::Subagent, "a"),
        ] {
            state
                .entries
                .get_mut(&key)
                .unwrap()
                .insert(id.to_string(), entry(kind, id, json!({})));
        }
        state
    }

    fn window(proposal_id: &str, overrides: &Value) -> Value {
        let mut window = json!({
            "proposalId": proposal_id,
            "touched": ["skill:s"],
            "claimedFingerprints": [FP],
            "committedTurn": 10,
            "untilTurn": 30,
            "outcome": "open",
            "skillImports": {"skill:s": ["pkg.mod"]}
        });
        for (key, value) in overrides.as_object().unwrap() {
            if value.is_null() {
                window.as_object_mut().unwrap().remove(key);
            } else {
                window[key] = value.clone();
            }
        }
        window
    }

    fn windows_of(list: &[Value]) -> TrustWindows {
        let map: serde_json::Map<String, Value> = list
            .iter()
            .map(|window| {
                (
                    window["proposalId"].as_str().unwrap().to_string(),
                    window.clone(),
                )
            })
            .collect();
        normalize_trust_windows(Some(&Value::Object(map))).unwrap()
    }

    fn recurrence_of(proposal_id: &str, ordinal: u64, cases: Vec<ReplayCase>) -> TrustRecurrence {
        TrustRecurrence {
            proposal_id: proposal_id.to_string(),
            fingerprint_id: FP.to_string(),
            ordinal,
            observed_cases: cases,
        }
    }

    fn plan_with(
        windows: &TrustWindows,
        recurrences: &[TrustRecurrence],
        record: Option<FailureRecord>,
        skills: &[(&str, &str)],
    ) -> (Vec<TrustAdjudicationJob>, Vec<AwaitingTrustAdjudication>) {
        let record = record.unwrap_or_else(|| failure_record(vec![probe("import pkg", true)]));
        let state = entries(skills);
        let record_of = |id: &str| (id == record.fingerprint.id).then(|| record.clone());
        plan_trust_adjudications(&TrustPlanInput {
            scope: HarnessScope::Local,
            windows: Some(windows),
            recurrences,
            entries: &state,
            record_of: &record_of,
        })
    }

    fn plan(
        windows: &TrustWindows,
        recurrences: &[TrustRecurrence],
    ) -> (Vec<TrustAdjudicationJob>, Vec<AwaitingTrustAdjudication>) {
        plan_with(windows, recurrences, None, &[("s", "pkg.mod")])
    }

    fn proposal_ids(jobs: &[TrustAdjudicationJob]) -> Vec<&str> {
        jobs.iter().map(|job| job.proposal_id.as_str()).collect()
    }

    #[test]
    fn recurrences_are_found_only_for_open_windows_that_claimed_and_hold_the_ordinal() {
        let windows = windows_of(&[
            window("refine_b", &json!({"claimedFingerprints": [OTHER_FP, FP]})),
            window("refine_a", &json!({})),
            window("refine_clean", &json!({"outcome": "clean"})),
            window("refine_contested", &json!({"outcome": "contested"})),
            window("refine_faulted", &json!({"outcome": "faulted"})),
            window("refine_unmeasured", &json!({"outcome": "unmeasured"})),
            window(
                "refine_unclaimed",
                &json!({"claimedFingerprints": ["fp_never"]}),
            ),
            window(
                "refine_late",
                &json!({"committedTurn": 13, "untilTurn": 33}),
            ),
            window(
                "refine_early",
                &json!({"committedTurn": 0, "untilTurn": 11}),
            ),
        ]);
        let recurred: IndexMap<String, Vec<ReplayCase>> = [
            (
                FP.to_string(),
                vec![
                    probe("import pkg", false),
                    probe("import pkg", false),
                    probe("from pkg import x", false),
                ],
            ),
            (OTHER_FP.to_string(), Vec::new()),
        ]
        .into_iter()
        .collect();
        let found = find_trust_window_recurrences(Some(&windows), &recurred, 12);
        let ids: Vec<(&str, &str)> = found
            .iter()
            .map(|item| (item.proposal_id.as_str(), item.fingerprint_id.as_str()))
            .collect();
        assert_eq!(
            ids,
            [("refine_a", FP), ("refine_b", FP), ("refine_b", OTHER_FP)]
        );
        assert_eq!(found[0].observed_cases, [probe("import pkg", false)]);
        assert_eq!(found[0].ordinal, 12);
        assert!(found[2].observed_cases.is_empty());
        assert!(find_trust_window_recurrences(None, &recurred, 12).is_empty());
    }

    #[test]
    fn a_replay_is_planned_only_for_a_skill_still_importing_what_its_commit_recorded() {
        let (jobs, awaiting) = plan(
            &windows_of(&[window("refine_a", &json!({}))]),
            &[recurrence_of(
                "refine_a",
                12,
                vec![probe("import pkg", false)],
            )],
        );
        assert!(awaiting.is_empty());
        assert_eq!(
            jobs,
            [TrustAdjudicationJob {
                scope: HarnessScope::Local,
                proposal_id: "refine_a".to_string(),
                entry: "skill:s".to_string(),
                fingerprint_id: FP.to_string(),
                ordinal: 12,
                skill_imports: vec!["pkg.mod".to_string()],
                record: failure_record(vec![probe("import pkg", true)]),
            }]
        );
        assert_eq!(jobs[0].key(), format!("local refine_a skill:s {FP}"));

        let nothing = |overrides: Value, skills: &[(&str, &str)]| {
            let planned = plan_with(
                &windows_of(&[window("refine_a", &overrides)]),
                &[recurrence_of(
                    "refine_a",
                    12,
                    vec![probe("import pkg", false)],
                )],
                None,
                skills,
            );
            assert_eq!(planned, (Vec::new(), Vec::new()));
        };
        let skill = [("s", "pkg.mod")];
        nothing(
            json!({"touched": ["memory:m", "prompt:p", "subagent:a"], "skillImports": {"memory:m": ["pkg.mod"]}}),
            &skill,
        );
        nothing(json!({"skillImports": null}), &skill);
        nothing(json!({}), &[("s", "pkg.other")]);
        nothing(json!({}), &[]);
        let runs = |count: usize| -> Vec<String> {
            (0..count)
                .map(|index| format!("2026-09-16T10:00:0{index}.000Z"))
                .collect()
        };
        nothing(
            json!({"adjudications": [{"entry": "skill:s", "fingerprintId": FP, "status": "upheld", "ordinal": 11, "runs": runs(1)}]}),
            &skill,
        );
        nothing(
            json!({"adjudications": [{"entry": "skill:s", "fingerprintId": FP, "status": "cleared", "ordinal": 11, "runs": runs(MAX_TRUST_ADJUDICATION_RUNS)}]}),
            &skill,
        );
        nothing(json!({"outcome": "faulted"}), &skill);
        let (partial, _) = plan(
            &windows_of(&[window(
                "refine_a",
                &json!({"adjudications": [{"entry": "skill:s", "fingerprintId": FP, "status": "cleared", "ordinal": 11, "runs": runs(2)}]}),
            )]),
            &[recurrence_of(
                "refine_a",
                12,
                vec![probe("import pkg", false)],
            )],
        );
        assert_eq!(partial.len(), 1);
    }

    #[test]
    fn another_module_failing_under_the_same_fingerprint_plans_nothing() {
        let windows = windows_of(&[window("refine_a", &json!({}))]);
        assert_eq!(
            plan_with(
                &windows,
                &[recurrence_of(
                    "refine_a",
                    12,
                    vec![probe("import other", false)]
                )],
                Some(failure_record(vec![
                    probe("import pkg", true),
                    probe("import other", false)
                ])),
                &[("s", "pkg.mod")],
            ),
            (Vec::new(), Vec::new())
        );
        assert_eq!(
            plan_with(
                &windows,
                &[recurrence_of(
                    "refine_a",
                    12,
                    vec![probe("from pkg import mod", false)]
                )],
                Some(failure_record(vec![probe("from pkg import mod", true)])),
                &[("s", "pkg.mod")],
            ),
            (Vec::new(), Vec::new())
        );
        assert_eq!(
            plan(&windows, &[recurrence_of("refine_a", 12, Vec::new())]),
            (Vec::new(), Vec::new())
        );
    }

    #[test]
    fn an_overlapping_replay_fact_goes_to_the_newest_window_that_wrote_the_imports() {
        let older = window("refine_w1", &json!({"committedTurn": 10, "untilTurn": 30}));
        let newer = window("refine_w2", &json!({"committedTurn": 13, "untilTurn": 33}));
        let observed = || vec![probe("import pkg", false)];
        let windows = windows_of(&[older.clone(), newer.clone()]);
        let (at15, _) = plan(
            &windows,
            &[
                recurrence_of("refine_w1", 15, observed()),
                recurrence_of("refine_w2", 15, observed()),
            ],
        );
        assert_eq!(proposal_ids(&at15), ["refine_w2"]);
        let (at12, _) = plan(&windows, &[recurrence_of("refine_w1", 12, observed())]);
        assert_eq!(proposal_ids(&at12), ["refine_w1"]);
        let mut rewrote = newer.clone();
        rewrote["skillImports"] = json!({"skill:s": ["pkg.other"]});
        let (kept, _) = plan(
            &windows_of(&[older.clone(), rewrote]),
            &[recurrence_of("refine_w1", 15, observed())],
        );
        assert_eq!(proposal_ids(&kept), ["refine_w1"]);
        let mut faulted = newer;
        faulted["outcome"] = json!("faulted");
        assert_eq!(
            plan(
                &windows_of(&[older, faulted]),
                &[recurrence_of("refine_w1", 15, observed())]
            ),
            (Vec::new(), Vec::new())
        );
    }

    #[test]
    fn a_job_waits_for_the_self_check_to_verify_its_observed_case() {
        let record = failure_record(vec![probe("import pkg", false)]);
        let (jobs, awaiting) = plan_with(
            &windows_of(&[window("refine_a", &json!({}))]),
            &[recurrence_of(
                "refine_a",
                12,
                vec![probe("import pkg", false)],
            )],
            Some(record),
            &[("s", "pkg.mod")],
        );
        assert!(jobs.is_empty());
        assert_eq!(awaiting.len(), 1);
        assert_eq!(awaiting[0].sources, ["import pkg"]);
        let verification = |fingerprint_id: &str, source: &str| ReplayVerification {
            fingerprint_id: fingerprint_id.to_string(),
            source: source.to_string(),
            verified_at: VERIFIED_AT.to_string(),
        };
        assert_eq!(
            release_awaiting_trust_adjudication(
                &awaiting[0],
                &[
                    verification(FP, "import other"),
                    verification(OTHER_FP, "import pkg")
                ]
            ),
            ReleasedAdjudication {
                matched: false,
                job: None
            }
        );
        let released =
            release_awaiting_trust_adjudication(&awaiting[0], &[verification(FP, "import pkg")]);
        assert_eq!(
            released,
            ReleasedAdjudication {
                matched: true,
                job: Some(TrustAdjudicationJob {
                    record: failure_record(vec![probe("import pkg", true)]),
                    ..awaiting[0].job.clone()
                }),
            }
        );
        assert_eq!(
            awaiting[0].job.record.replay_cases,
            [probe("import pkg", false)]
        );
    }

    #[test]
    fn jobs_and_awaiting_are_capped() {
        let ids: Vec<String> = (0..MAX_TRUST_ADJUDICATION_JOBS + 3)
            .map(|index| format!("s{index:02}"))
            .collect();
        let touched: Vec<String> = ids.iter().map(|id| format!("skill:{id}")).collect();
        let imports: serde_json::Map<String, Value> = touched
            .iter()
            .map(|entry| (entry.clone(), json!(["pkg.mod"])))
            .collect();
        let skills: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), "pkg.mod")).collect();
        let windows = windows_of(&[window(
            "refine_a",
            &json!({"touched": touched, "skillImports": imports}),
        )]);
        let observed = || {
            vec![recurrence_of(
                "refine_a",
                12,
                vec![probe("import pkg", false)],
            )]
        };
        let (jobs, awaiting) = plan_with(&windows, &observed(), None, &skills);
        assert_eq!(
            (jobs.len(), awaiting.len()),
            (MAX_TRUST_ADJUDICATION_JOBS, 0)
        );
        let (jobs, awaiting) = plan_with(
            &windows,
            &observed(),
            Some(failure_record(vec![probe("import pkg", false)])),
            &skills,
        );
        assert_eq!(
            (jobs.len(), awaiting.len()),
            (0, MAX_TRUST_ADJUDICATION_JOBS)
        );
    }

    /// Answers every run with one outcome and records the environments.
    struct Answers {
        outcome: ReplayOutcome,
        environments: Mutex<Vec<ReplayEnvironment>>,
        abort_after_first: Option<&'static AtomicBool>,
    }

    impl ReplayRunner for Answers {
        fn run<'a>(
            &'a self,
            _case: &'a ReplayCase,
            environment: ReplayEnvironment,
            _sys_path: &'a [String],
        ) -> Pin<Box<dyn Future<Output = ReplayOutcome> + Send + 'a>> {
            self.environments.lock().unwrap().push(environment);
            if let Some(flag) = self.abort_after_first {
                flag.store(true, Ordering::SeqCst);
            }
            let outcome = self.outcome.clone();
            Box::pin(async move { outcome })
        }
    }

    static ABORT: AtomicBool = AtomicBool::new(false);

    fn job(proposal_id: &str) -> TrustAdjudicationJob {
        TrustAdjudicationJob {
            scope: HarnessScope::Global,
            proposal_id: proposal_id.to_string(),
            entry: "skill:s".to_string(),
            fingerprint_id: FP.to_string(),
            ordinal: 12,
            skill_imports: vec!["prime_agent_trust_absent_mod".to_string()],
            record: failure_record(vec![probe("import prime_agent_trust_absent_mod", true)]),
        }
    }

    /// An upheld replay is evidence on its own window and entry; a batch
    /// aborted during its run yields nothing.
    #[tokio::test]
    async fn verdicts_become_scoped_evidence_and_an_aborted_batch_yields_none() {
        let runner = Answers {
            outcome: ReplayOutcome::Raised {
                exception_class: "ModuleNotFoundError".to_string(),
                detail: "No module named 'prime_agent_trust_absent_mod'".to_string(),
            },
            environments: Mutex::new(Vec::new()),
            abort_after_first: None,
        };
        let aborted = AtomicBool::new(false);
        let now = || "2026-09-16T09:00:00.000Z".to_string();
        let evidence = adjudicate_trust_recurrences(
            &[job("refine_a")],
            TrustAdjudicationRun {
                runner: &runner,
                sys_path: &[],
                session_id: "s1",
                aborted: &aborted,
                now: &now,
            },
        )
        .await;
        assert_eq!(
            evidence,
            [ScopedTrustEvidence {
                scope: HarnessScope::Global,
                evidence: TrustWindowEvidence::Adjudication {
                    proposal_id: "refine_a".to_string(),
                    entry: "skill:s".to_string(),
                    fingerprint_id: FP.to_string(),
                    status: TrustAdjudicationStatus::Upheld,
                    ordinal: 12,
                    at: "2026-09-16T09:00:00.000Z".to_string(),
                },
            }]
        );
        assert_eq!(
            *runner.environments.lock().unwrap(),
            [ReplayEnvironment::SkillImport]
        );

        let aborting = Answers {
            outcome: ReplayOutcome::Clean {
                detail: String::new(),
            },
            environments: Mutex::new(Vec::new()),
            abort_after_first: Some(&ABORT),
        };
        let evidence = adjudicate_trust_recurrences(
            &[job("refine_a"), job("refine_b")],
            TrustAdjudicationRun {
                runner: &aborting,
                sys_path: &[],
                session_id: "s1",
                aborted: &ABORT,
                now: &now,
            },
        )
        .await;
        assert!(evidence.is_empty());
        assert_eq!(aborting.environments.lock().unwrap().len(), 1);
    }
}
