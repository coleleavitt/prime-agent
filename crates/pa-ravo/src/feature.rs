//! The RAVO session feature: the gate every session's refinements meet
//! (through `pa_core`'s refinement-gate seam) and the failure-ledger
//! observer that records provisional regressions into the lineages and
//! holds a session's ledger flushes while one of its refines runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use pa_core::features::{FeatureFuture, FeatureTelemetry, SessionFeature, SessionFeatureContext};
use pa_core::refinement::gate::{
    GateAdmission, RefineGuard, RefinementGate, RefinementGateRequest, RefinementGateVerdict,
};
use pa_core::refinement::planner::{refused_refinement_edits, RefinementProposal};
use pa_core::refinement::ranking::{format_harness_state_for_prompt, HarnessStatePromptOptions};
use pa_core::refinement::{HarnessScope, HarnessState, RefinementResult};
use pa_core::session_engine::refine::RefinementSource;
use pa_ledger::{
    find_provisional_regressions, local_harness_state_dir, normalize_failure_ledger,
    observation_ordinal, record_provisional_regressions, recurring_failures, FailureLedger,
    FailureRecord, HarnessDocument, LedgerBoundary, LedgerFlush, LedgerHandle, LedgerObserver,
    LedgerScope, ProvisionalRegression,
};
use pa_telemetry::Properties;
use pa_types::trace_context::SPAN_ATTRIBUTES_TARGET;
use serde_json::{Map, Value};

use crate::authority::{assisted_ravo_binding_matches, DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS};
use crate::gate::{
    carry_observed_recurrences, gate_start_state, judge_conversation_text, proposal_artifact,
    ravo_evaluate_proposal, refinement_baseline_view, refinement_rejection_cause, scope_name,
    set_stored_ravo_state, stored_ravo_state, GateEvaluation, RavoDecision, RavoGateReport,
    RefineReason, RejectionCause, RAVO_BASELINE_CHANGED_RATIONALE, RAVO_DEFAULT_CONFIG, RAVO_KEY,
};
use crate::reducer::RavoWindowClock;
use crate::referee::ReplayRunner;
use crate::verification::ReplayVerifier;

/// The kill switch: gating is on unless it says `0`, `off` or `false`.
pub const RAVO_ENV: &str = "PRIME_AGENT_RAVO";

/// The adoption event: one gated refinement's final decision.
pub const RAVO_GATE_DECISION_EVENT: &str = "ravo_gate_decision";

/// Where the outcome log lines go (TS `REFINEMENT_LOG_COMPONENT`).
pub const REFINEMENT_LOG_TARGET: &str = "pa_ravo::refinement";

/// Whether RAVO gating is on for a `PRIME_AGENT_RAVO` value.
#[must_use]
pub fn ravo_enabled(value: Option<&str>) -> bool {
    let value = value.map(|value| value.trim().to_lowercase());
    !matches!(value.as_deref(), Some("0" | "off" | "false"))
}

/// How the feature behaves.
#[derive(Clone)]
pub struct RavoOptions {
    /// Gate refinements; `None` reads `PRIME_AGENT_RAVO` at each refine.
    pub enabled: Option<bool>,
    /// Runs the referee's replay cases.
    pub runner: Arc<dyn ReplayRunner>,
    /// Extra `sys.path` roots replays import with.
    pub replay_sys_path: Vec<String>,
}

/// Provisional regressions found at boundaries, waiting for the flush of
/// the scope whose lineage they belong to.
#[derive(Default)]
struct PendingRegressions {
    local: Vec<(Vec<ProvisionalRegression>, u64)>,
    global: Vec<(Vec<ProvisionalRegression>, u64)>,
    /// How many of each list the announced flush recorded.
    local_recorded: usize,
    global_recorded: usize,
}

struct Inner {
    options: RavoOptions,
    ledger: OnceLock<LedgerHandle>,
    /// Refines running per session.
    refines: Mutex<HashMap<String, usize>>,
    regressions: Mutex<HashMap<String, PendingRegressions>>,
    verifier: Arc<ReplayVerifier>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The RAVO feature (`pa-cli` installs it behind `feature = "ravo"`).
#[derive(Clone)]
pub struct RavoFeature {
    inner: Arc<Inner>,
}

impl RavoFeature {
    /// The feature with `options`; attach the ledger with
    /// [`Self::attach_ledger`] once the ledger feature exists.
    #[must_use]
    pub fn new(options: RavoOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                options,
                ledger: OnceLock::new(),
                refines: Mutex::new(HashMap::new()),
                regressions: Mutex::new(HashMap::new()),
                verifier: Arc::default(),
            }),
        }
    }

    /// The observer to build the ledger feature with.
    #[must_use]
    pub fn ledger_observer(&self) -> Arc<dyn LedgerObserver> {
        Arc::new(RavoLedgerObserver {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Wait until no replay self-check runs, up to `timeout`; `false` when
    /// the timeout passed first.
    #[must_use]
    pub fn wait_replay_checks(&self, timeout: std::time::Duration) -> bool {
        self.inner
            .verifier
            .wait_idle(std::time::Instant::now() + timeout)
    }

    /// Give the feature the ledger's handle; later calls are ignored.
    pub fn attach_ledger(&self, handle: LedgerHandle) {
        let _ = self.inner.ledger.set(handle);
    }
}

impl Inner {
    fn enabled(&self) -> bool {
        self.options
            .enabled
            .unwrap_or_else(|| ravo_enabled(std::env::var(RAVO_ENV).ok().as_deref()))
    }
}

impl SessionFeature for RavoFeature {
    fn name(&self) -> &'static str {
        "ravo"
    }

    /// Let running replay self-checks finish, so the ledger's exit flush
    /// (installed after this feature) writes what they verified.
    fn flush(&self, deadline: std::time::Instant) {
        if !self.inner.verifier.wait_idle(deadline) {
            tracing::debug!("replay self-checks abandoned at the exit deadline");
        }
    }

    fn refinement_gate(
        &self,
        context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn RefinementGate>> {
        Some(Arc::new(SessionGate {
            inner: Arc::clone(&self.inner),
            context: Arc::clone(context),
        }))
    }
}

/// One session's gate.
struct SessionGate {
    inner: Arc<Inner>,
    context: Arc<SessionFeatureContext>,
}

/// Holds the session's ledger flushes while a refine runs.
struct RefineHold {
    inner: Arc<Inner>,
    session_id: String,
}

impl Drop for RefineHold {
    fn drop(&mut self) {
        {
            let mut refines = lock(&self.inner.refines);
            if let Some(count) = refines.get_mut(&self.session_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    refines.remove(&self.session_id);
                }
            }
        }
        // What the held flushes skipped lands now.
        if let Some(ledger) = self.inner.ledger.get() {
            ledger.request_flush(&self.session_id);
        }
    }
}

fn refine_reason(source: RefinementSource) -> RefineReason {
    match source {
        RefinementSource::User => RefineReason::Manual,
        RefinementSource::Auto => RefineReason::Compact,
        RefinementSource::SelfRefine => RefineReason::RefineRun,
    }
}

/// What a non-failure refine is charged (TS `_gateRecurringFailures`):
/// the fingerprints recurring in `ledger` that also recur in the session's
/// own ledger and were last seen within the window behind `turn`.
fn gate_recurring_failures(
    ledger: &FailureLedger,
    session_ledger: &FailureLedger,
    turn: u64,
) -> Vec<FailureRecord> {
    let charged: Vec<String> = recurring_failures(session_ledger, None)
        .into_iter()
        .filter(|record| {
            turn.checked_sub(record.last_seen_turn)
                .is_some_and(|age| age <= DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS)
        })
        .map(|record| record.fingerprint.id)
        .collect();
    recurring_failures(ledger, None)
        .into_iter()
        .filter(|record| charged.contains(&record.fingerprint.id))
        .collect()
}

fn state_failures(state: &HarnessState) -> FailureLedger {
    state
        .extensions
        .get(pa_ledger::FAILURES_KEY)
        .map(normalize_failure_ledger)
        .unwrap_or_default()
}

impl SessionGate {
    /// The recurring failures the refine is charged and the clock its
    /// window opens on (TS `_gateRecurringFailures` and
    /// `_provisionalWindowClock`).
    fn charges(
        &self,
        scope: HarnessScope,
        baseline: &HarnessState,
        planning: &HarnessState,
    ) -> (Vec<FailureRecord>, Option<u64>, Option<RavoWindowClock>) {
        let session_id = self.context.session_id.as_str();
        let ledger = self.inner.ledger.get();
        let session_ledger = ledger
            .and_then(|ledger| ledger.session_ledger(session_id))
            .unwrap_or_else(|| state_failures(planning));
        let turn = ledger
            .and_then(|ledger| ledger.turn(session_id))
            .unwrap_or(0);
        let global = ledger
            .filter(|ledger| ledger.global_ledger_enabled())
            .map(|ledger| ledger.fresh_global_ledger(&self.context.agent_dir, Some(session_id)));
        let charged_against = global.clone().unwrap_or_else(|| state_failures(baseline));
        let recurring = gate_recurring_failures(&charged_against, &session_ledger, turn);
        let (turn, clock) = match (&global, scope) {
            (Some(global), _) => (
                Some(observation_ordinal(Some(global))),
                Some(RavoWindowClock::Ordinal),
            ),
            (None, HarnessScope::Local) => (
                Some(observation_ordinal(Some(&session_ledger))),
                Some(RavoWindowClock::LocalOrdinal),
            ),
            (None, HarnessScope::Global) => (
                ledger.map(|ledger| {
                    observation_ordinal(Some(
                        &ledger.fresh_global_ledger(&self.context.agent_dir, Some(session_id)),
                    ))
                }),
                None,
            ),
        };
        (recurring, turn, clock)
    }
}

impl RefinementGate for SessionGate {
    fn begin_refine(&self) -> Option<RefineGuard> {
        let session_id = self.context.session_id.clone();
        *lock(&self.inner.refines)
            .entry(session_id.clone())
            .or_insert(0) += 1;
        Some(Box::new(RefineHold {
            inner: Arc::clone(&self.inner),
            session_id,
        }))
    }

    fn evaluate(
        &self,
        request: RefinementGateRequest,
    ) -> FeatureFuture<anyhow::Result<Option<Box<dyn RefinementGateVerdict>>>> {
        if !self.inner.enabled() {
            return Box::pin(async { Ok(None) });
        }
        let reason = refine_reason(request.source);
        let (recurring, turn, clock) = self.charges(
            request.scope,
            &request.baseline_state,
            &request.planning_state,
        );
        let inner = Arc::clone(&self.inner);
        let telemetry = self.context.telemetry.clone();
        Box::pin(async move {
            let stored = stored_ravo_state(&request.baseline_state);
            let state = gate_start_state(stored.as_ref());
            let report = ravo_evaluate_proposal(
                GateEvaluation {
                    proposal: &request.proposal,
                    proposal_id: &request.proposal_id,
                    state: &state,
                    config: RAVO_DEFAULT_CONFIG,
                    conversation_text: judge_conversation_text(&request.messages),
                    harness_overview: format_harness_state_for_prompt(
                        &request.planning_state,
                        &HarnessStatePromptOptions {
                            include_ipython_examples: Some(false),
                            ..HarnessStatePromptOptions::default()
                        },
                    ),
                    baseline: refinement_baseline_view(&request.baseline_state),
                    recurring_failures: &recurring,
                    turn,
                    turn_clock: clock,
                    refine_kind: reason.kind(),
                    model: request.model.clone(),
                    runner: inner.options.runner.as_ref(),
                    sys_path: &inner.options.replay_sys_path,
                },
                request.model_call,
            )
            .await;
            Ok(Some(Box::new(RavoVerdict {
                report,
                proposal_id: request.proposal_id,
                scope: request.scope,
                reason,
                telemetry,
                rejection: Mutex::new(None),
            }) as Box<dyn RefinementGateVerdict>))
        })
    }
}

/// A refused proposal's bookkeeping, settled by `admit`.
struct Rejection {
    binding_matches: bool,
    report: RavoGateReport,
    cause: RejectionCause,
}

/// One evaluated proposal, consulted while it applies.
struct RavoVerdict {
    report: RavoGateReport,
    proposal_id: String,
    scope: HarnessScope,
    reason: RefineReason,
    telemetry: Option<FeatureTelemetry>,
    rejection: Mutex<Option<Rejection>>,
}

/// A refinement's final decision, after the apply-time checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinalDecision {
    Gate(RavoDecision),
    CommitUnmeasured,
    Partial,
}

impl FinalDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Gate(decision) => decision.as_str(),
            Self::CommitUnmeasured => "commit_unmeasured",
            Self::Partial => "partial",
        }
    }
}

impl RavoVerdict {
    /// One structured line for the final decision (TS
    /// `logRefinementOutcome`), and the adoption event.
    fn report_outcome(
        &self,
        decision: FinalDecision,
        report: &RavoGateReport,
        cause: Option<RejectionCause>,
    ) {
        let proposal_id = self.proposal_id.as_str();
        let reason = self.reason.as_str();
        let scope = scope_name(self.scope);
        let addressed = report.addressed_fingerprints.join(",");
        match decision {
            FinalDecision::Gate(RavoDecision::Commit)
                if !report.addressed_fingerprints.is_empty() =>
            {
                tracing::info!(
                    target: REFINEMENT_LOG_TARGET,
                    proposal_id,
                    addressed,
                    deep_score = report.deep_score,
                    missed = report.missed_criteria.len(),
                    reason,
                    scope,
                    "refinement.committed"
                );
            }
            FinalDecision::Gate(RavoDecision::Commit) | FinalDecision::CommitUnmeasured => {
                tracing::info!(
                    target: REFINEMENT_LOG_TARGET,
                    proposal_id,
                    deep_score = report.deep_score,
                    reason,
                    scope,
                    "refinement.applied_unmeasured"
                );
            }
            FinalDecision::Gate(_) | FinalDecision::Partial => {
                tracing::info!(
                    target: REFINEMENT_LOG_TARGET,
                    proposal_id,
                    decision = decision.as_str(),
                    deep_score = report.deep_score,
                    missed = report.missed_criteria.len(),
                    claimed = report.addressed_fingerprints.len(),
                    reason,
                    scope,
                    cause = cause.map(RejectionCause::as_str),
                    "refinement.rejected"
                );
            }
        }
        if let Some(telemetry) = &self.telemetry {
            let mut properties = Properties::new();
            properties.set("decision", Value::from(decision.as_str()));
            properties.set("scope", Value::from(scope));
            properties.set("reason", Value::from(reason));
            properties.set(
                "cause",
                Value::from(cause.map_or("none", RejectionCause::as_str)),
            );
            properties.set(
                "recurring",
                Value::from(report.failure_opponents.len() as u64),
            );
            properties.set(
                "claimed",
                Value::from(report.addressed_fingerprints.len() as u64),
            );
            telemetry.track(RAVO_GATE_DECISION_EVENT, &properties);
        }
    }

    fn rejected_result(
        &self,
        proposal: &RefinementProposal,
        report: &RavoGateReport,
        cause: RejectionCause,
    ) -> RefinementResult {
        let error = format!(
            "ravo gate rejected ({}): {}",
            report.decision.as_str(),
            report.rationale
        );
        let mut extensions = Map::new();
        extensions.insert(
            RAVO_KEY.to_string(),
            serde_json::to_value(report).unwrap_or(Value::Null),
        );
        extensions.insert("rejectionCause".to_string(), Value::from(cause.as_str()));
        RefinementResult {
            id: self.proposal_id.clone(),
            summary: format!("RAVO gate rejected: {}", proposal.summary),
            rationale: proposal.rationale.clone(),
            expected_outcome: proposal.expected_outcome.clone(),
            applied_edits: refused_refinement_edits(proposal, &error),
            harness_state_path: String::new(),
            rollback_of: None,
            scope: Some(self.scope),
            extensions,
        }
    }
}

impl RefinementGateVerdict for RavoVerdict {
    fn admit(&self, proposal: &RefinementProposal, current: &HarnessState) -> GateAdmission {
        let artifact = proposal_artifact(proposal);
        let baseline = refinement_baseline_view(current);
        let authorization = self.report.authorization.as_ref();
        let binding_matches = authorization.is_some_and(|authorization| {
            assisted_ravo_binding_matches(authorization, &artifact, &baseline)
        });
        let authorized = binding_matches && authorization.is_some_and(|auth| auth.authorized);
        if self.report.decision == RavoDecision::Commit && authorized {
            return GateAdmission::Apply;
        }
        // Only an approval can be lost here; a gate rejection keeps its own
        // report whether or not the harness moved.
        let approval_lost = self.report.decision == RavoDecision::Commit;
        let mut report = self.report.clone();
        if approval_lost {
            report.decision = RavoDecision::RejectDeep;
            report.rationale = RAVO_BASELINE_CHANGED_RATIONALE.to_string();
        }
        let cause = refinement_rejection_cause(&report, approval_lost);
        let rejected = self.rejected_result(proposal, &report, cause);
        *lock(&self.rejection) = Some(Rejection {
            binding_matches,
            report,
            cause,
        });
        GateAdmission::Reject(Box::new(rejected))
    }

    fn record_rejection(&self, state: &mut HarnessState) -> bool {
        let Some(rejection) = lock(&self.rejection).take() else {
            return false;
        };
        let next = self
            .report
            .authorization
            .as_ref()
            .filter(|_| rejection.binding_matches)
            .map(|authorization| {
                carry_observed_recurrences(
                    &authorization.next_state,
                    stored_ravo_state(state).as_ref(),
                )
            });
        self.report_outcome(
            FinalDecision::Gate(rejection.report.decision),
            &rejection.report,
            Some(rejection.cause),
        );
        // The consumed evaluation is persisted so the proposal id cannot be
        // evaluated twice.
        let Some(next) = next else {
            return false;
        };
        set_stored_ravo_state(state, &next);
        true
    }

    fn record_application(&self, state: &mut HarnessState, result: &mut RefinementResult) {
        let all_applied = result.applied_edits.iter().all(|edit| edit.applied);
        if let (true, Some(authorization)) = (all_applied, self.report.authorization.as_ref()) {
            // A regression another session recorded while this one planned
            // is not bound, so it survives.
            let next = carry_observed_recurrences(
                &authorization.next_state,
                stored_ravo_state(state).as_ref(),
            );
            set_stored_ravo_state(state, &next);
            result.extensions.insert(
                RAVO_KEY.to_string(),
                serde_json::to_value(&self.report).unwrap_or(Value::Null),
            );
        }
        let decision = if !all_applied {
            FinalDecision::Partial
        } else if self.report.measurable {
            FinalDecision::Gate(RavoDecision::Commit)
        } else {
            FinalDecision::CommitUnmeasured
        };
        self.report_outcome(decision, &self.report, None);
    }
}

/// The ledger observer half.
struct RavoLedgerObserver {
    inner: Arc<Inner>,
}

impl LedgerObserver for RavoLedgerObserver {
    fn on_boundary(&self, context: &Arc<SessionFeatureContext>, boundary: &LedgerBoundary<'_>) {
        if let Some(handle) = self.inner.ledger.get() {
            self.inner.verifier.observe(
                &context.session_id,
                boundary.observations,
                boundary.effective,
                Arc::clone(&self.inner.options.runner),
                handle.clone(),
            );
        }
        if boundary.recurred_ids.is_empty() {
            return;
        }
        let Some(artifact_dir) = context.session_artifact_dir.as_deref() else {
            return;
        };
        // Each window is checked on the clock it was stamped with.
        let local = HarnessDocument::load(&local_harness_state_dir(artifact_dir));
        let local_ravo = local.get(RAVO_KEY);
        let on_local_clock = find_provisional_regressions(
            local_ravo,
            boundary.recurred_ids,
            boundary.local_ordinal,
            RavoWindowClock::LocalOrdinal.as_str(),
        );
        let on_global_clock = boundary.global_ordinal.map(|ordinal| {
            find_provisional_regressions(
                local_ravo,
                boundary.recurred_ids,
                ordinal,
                RavoWindowClock::Ordinal.as_str(),
            )
        });
        let global =
            boundary
                .global_ordinal
                .zip(boundary.global_state)
                .map(|(ordinal, document)| {
                    (
                        find_provisional_regressions(
                            document.get(RAVO_KEY),
                            boundary.recurred_ids,
                            ordinal,
                            RavoWindowClock::Ordinal.as_str(),
                        ),
                        ordinal,
                    )
                });
        let mut pending = lock(&self.inner.regressions);
        let pending = pending.entry(context.session_id.clone()).or_default();
        if !on_local_clock.is_empty() {
            pending.local.push((on_local_clock, boundary.local_ordinal));
        }
        if let (Some(regressions), Some(ordinal)) = (on_global_clock, boundary.global_ordinal) {
            if !regressions.is_empty() {
                pending.local.push((regressions, ordinal));
            }
        }
        if let Some((regressions, ordinal)) = global {
            if !regressions.is_empty() {
                pending.global.push((regressions, ordinal));
            }
        }
    }

    fn hold_flush(&self, session_id: &str) -> bool {
        lock(&self.inner.refines).contains_key(session_id)
    }

    fn wants_global_flush(&self, session_id: &str) -> bool {
        lock(&self.inner.regressions)
            .get(session_id)
            .is_some_and(|pending| !pending.global.is_empty())
    }

    fn on_flush(&self, flush: &mut LedgerFlush<'_>) {
        let mut regressions = lock(&self.inner.regressions);
        let Some(pending) = regressions.get_mut(flush.session_id) else {
            if flush.scope == LedgerScope::Global {
                tracing::event!(
                    target: SPAN_ATTRIBUTES_TARGET,
                    tracing::Level::INFO,
                    ledger.regressions = 0u64
                );
            }
            return;
        };
        let (list, recorded) = match flush.scope {
            LedgerScope::Local => (&pending.local, &mut pending.local_recorded),
            LedgerScope::Global => (&pending.global, &mut pending.global_recorded),
        };
        if flush.scope == LedgerScope::Global {
            tracing::event!(
                target: SPAN_ATTRIBUTES_TARGET,
                tracing::Level::INFO,
                ledger.regressions = list.len() as u64
            );
        }
        // A regression stays pending while the state has no lineage to
        // record it on.
        *recorded = 0;
        let Some(mut ravo) = flush.document.get(RAVO_KEY).cloned() else {
            return;
        };
        for (batch, turn) in list {
            ravo = record_provisional_regressions(&ravo, batch, *turn);
        }
        *recorded = list.len();
        flush.document.set(RAVO_KEY, ravo);
    }

    fn on_flush_result(&self, scope: LedgerScope, session_id: &str, landed: bool) {
        let mut regressions = lock(&self.inner.regressions);
        let Some(pending) = regressions.get_mut(session_id) else {
            return;
        };
        let (list, recorded) = match scope {
            LedgerScope::Local => (&mut pending.local, &mut pending.local_recorded),
            LedgerScope::Global => (&mut pending.global, &mut pending.global_recorded),
        };
        if landed {
            list.drain(..(*recorded).min(list.len()));
        }
        *recorded = 0;
    }
}
