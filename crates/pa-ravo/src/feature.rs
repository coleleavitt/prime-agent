//! The RAVO session feature: the gate every session's refinements meet
//! (through `pa_core`'s refinement-gate seam) and the failure-ledger
//! observer that records provisional regressions into the lineages and
//! holds a session's ledger flushes while one of its refines runs.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use pa_core::features::{FeatureFuture, FeatureTelemetry, SessionFeature, SessionFeatureContext};
use pa_core::refinement::gate::{
    GateAdmission, RefineGuard, RefinementGate, RefinementGateRequest, RefinementGateVerdict,
};
use pa_core::refinement::planner::{refused_refinement_edits, RefinementProposal};
use pa_core::refinement::ranking::{
    format_harness_state_for_prompt, HarnessRenderFilter, HarnessRenderFilters,
    HarnessStatePromptOptions,
};
use pa_core::refinement::{
    HarnessEntry, HarnessScope, HarnessState, RefinementAction, RefinementKind, RefinementResult,
};
use pa_core::session_engine::refine::RefinementSource;
use pa_core::session_engine::turn_boundary::{PendingRefine, RefineRequester};
use pa_ledger::{
    acquire_harness_state_lock, find_provisional_regressions,
    format_recurrence_refine_instructions, format_regression_refine_instructions,
    local_harness_state_dir, normalize_failure_ledger, observation_ordinal,
    record_provisional_regressions, recurring_failures, FailureLedger, FailureRecord,
    HarnessDocument, LedgerBoundary, LedgerFlush, LedgerHandle, LedgerObserver, LedgerScope,
    ProvisionalRegression,
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
use crate::trigger::{failure_refine, queue, read_trigger, FailureRequest, RequestKind};
use crate::trust::{
    empty_entry_trust, harness_entry_ref, is_dormant_trust, log_trust_settlement,
    normalize_entry_trust, open_trust_window, record_harness_trust_evidence, reference_imports,
    settle_harness_trust, stored_trust_windows, trust_windows_value, TrustClaim, TrustSettlement,
    TRUST_KEY, TRUST_WINDOWS_KEY,
};
use crate::trust_runtime::{TrustRunner, TrustTracker};
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
    /// The sessions that accept RAVO's own refines.
    requesters: Mutex<HashMap<String, RefineRequester>>,
    /// `regression:<fp>` / `recurrence:<fp>` already queued, per session.
    triggered: Mutex<HashMap<String, HashSet<String>>>,
    /// Requests parked behind a pending one of the other scope.
    parked: Mutex<HashMap<String, Vec<PendingRefine>>>,
    trust: Arc<TrustTracker>,
    /// The failures each session's running (evaluated, not yet finished)
    /// refines were queued for, one entry per refine.
    live_triggers: Mutex<HashMap<String, Vec<Vec<String>>>>,
    /// The `ravo.run` services.
    run_host: Arc<crate::run_host::RunHost>,
    /// Each session's agent dir (where its global store lives).
    agent_dirs: Mutex<HashMap<String, std::path::PathBuf>>,
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
        let runner = Arc::clone(&options.runner);
        let replay_sys_path = options.replay_sys_path.clone();
        let feature = Self {
            inner: Arc::new(Inner {
                options,
                ledger: OnceLock::new(),
                refines: Mutex::new(HashMap::new()),
                regressions: Mutex::new(HashMap::new()),
                verifier: Arc::default(),
                requesters: Mutex::new(HashMap::new()),
                triggered: Mutex::new(HashMap::new()),
                parked: Mutex::new(HashMap::new()),
                trust: Arc::default(),
                live_triggers: Mutex::new(HashMap::new()),
                run_host: Arc::new(crate::run_host::RunHost {
                    runner: Arc::clone(&runner),
                    replay_sys_path,
                    model: Mutex::new(Arc::new(|context: &SessionFeatureContext| {
                        Arc::new(crate::run_host::SessionModel::new(context))
                            as Arc<dyn crate::run::RavoModel>
                    })),
                    services: Mutex::new(HashMap::new()),
                }),
                agent_dirs: Mutex::new(HashMap::new()),
            }),
        };
        // A finished self-check batch releases the trust replays awaiting it.
        let weak = Arc::downgrade(&feature.inner);
        feature
            .inner
            .verifier
            .set_listener(Box::new(move |session_id, verifications| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if let Some(run) = inner.trust_runner() {
                    inner
                        .trust
                        .release(session_id, verifications, &inner.verifier, &run);
                }
            }));
        feature
    }

    /// The observer to build the ledger feature with.
    #[must_use]
    pub fn ledger_observer(&self) -> Arc<dyn LedgerObserver> {
        Arc::new(RavoLedgerObserver {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Wait until no replay self-check and no trust replay runs, up to
    /// `timeout`; `false` when the timeout passed first.
    #[must_use]
    pub fn wait_replay_checks(&self, timeout: std::time::Duration) -> bool {
        self.inner
            .wait_referee_runs(std::time::Instant::now() + timeout)
    }

    /// Prompt `ravo.run`'s children with the model `factory` builds for a
    /// session instead of the session model (tests script it).
    ///
    #[must_use]
    pub fn with_run_model(self, factory: crate::run_host::ModelFactory) -> Self {
        *self
            .inner
            .run_host
            .model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = factory;
        self
    }

    /// Give the feature the ledger's handle; later calls are ignored.
    pub fn attach_ledger(&self, handle: LedgerHandle) {
        let _ = self.inner.ledger.set(handle);
    }
}

impl Inner {
    /// What a trust replay batch runs with, once the ledger is attached.
    fn trust_runner(&self) -> Option<TrustRunner> {
        self.ledger.get().map(|handle| TrustRunner {
            runner: Arc::clone(&self.options.runner),
            sys_path: self.options.replay_sys_path.clone(),
            handle: handle.clone(),
        })
    }

    fn remember(&self, context: &SessionFeatureContext) {
        lock(&self.agent_dirs)
            .entry(context.session_id.clone())
            .or_insert_with(|| context.agent_dir.clone());
    }

    /// The observation ordinal of the global ledger as it stands now (TS
    /// `observationOrdinal(_freshGlobalFailureLedger())`): the clock trust
    /// windows are measured on, even with the global ledger off (nothing
    /// advances it then, so no window settles: the fail-closed direction).
    fn fresh_global_ordinal(&self, session_id: &str) -> u64 {
        let agent_dir = lock(&self.agent_dirs).get(session_id).cloned();
        match (self.ledger.get(), agent_dir) {
            (Some(ledger), Some(agent_dir)) => observation_ordinal(Some(
                &ledger.fresh_global_ledger(&agent_dir, Some(session_id)),
            )),
            _ => 0,
        }
    }

    /// Wait for the running replay self-checks and trust replays,
    /// including the batches they start (TS `_awaitRefereeRuns`).
    fn wait_referee_runs(&self, deadline: std::time::Instant) -> bool {
        self.verifier.wait_idle(deadline)
            && self.trust.wait_idle(deadline)
            && self.verifier.wait_idle(deadline)
    }

    /// A dropped request repaired nothing: the failures that queued it may
    /// queue a repair again, except those a running refine still carries
    /// (TS `_releaseRefineTriggers`). The requests parked behind it are
    /// dropped with it (TS `_dropPendingRefineRequests`).
    fn release_dropped(&self, session_id: &str, dropped: &PendingRefine) {
        let parked = lock(&self.parked).remove(session_id).unwrap_or_default();
        let held: Vec<String> = lock(&self.live_triggers)
            .get(session_id)
            .map(|live| live.iter().flatten().cloned().collect())
            .unwrap_or_default();
        let mut triggered = lock(&self.triggered);
        let Some(triggered) = triggered.get_mut(session_id) else {
            return;
        };
        for request in std::iter::once(dropped).chain(&parked) {
            let Some(request) = request.trigger.as_ref().and_then(read_trigger) else {
                continue;
            };
            for id in &request.trigger_fingerprint_ids {
                if held.contains(id) {
                    continue;
                }
                triggered.remove(&format!("recurrence:{id}"));
                triggered.remove(&format!("regression:{id}"));
            }
        }
    }

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

    /// `ravo.run`, `ravo.status` and `ravo.cancel` (the bundled `ravo`
    /// skill's host side).
    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut pa_core::kernel::shared::HostRequestHandlers,
    ) {
        crate::run_host::register(&self.inner.run_host, context, handlers);
    }

    /// Let running replay self-checks finish, so the ledger's exit flush
    /// (installed after this feature) writes what they verified.
    fn flush(&self, deadline: std::time::Instant) {
        if !self.inner.wait_referee_runs(deadline) {
            tracing::debug!("replay self-checks and trust replays abandoned at the exit deadline");
        }
    }

    /// An approved automatic review refines the global harness unless the
    /// reviewer asked for a local refine, while the gate judges refines
    /// (TS `4c99bf8c2`); with gating off the native policy stays.
    fn auto_refine_policy(
        &self,
        _context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn pa_core::refinement::executor::AutoRefinePolicy>> {
        self.inner
            .enabled()
            .then(|| Arc::new(crate::auto_refine::GlobalDefaultAutoRefine) as _)
    }

    /// Dormant entries leave the rendered harness (TS
    /// `formatHarnessStateForPrompt`).
    fn harness_render_filter(
        &self,
        _context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn HarnessRenderFilter>> {
        Some(Arc::new(DormantEntries))
    }

    fn refinement_gate(
        &self,
        context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn RefinementGate>> {
        self.inner.remember(context);
        Some(Arc::new(SessionGate {
            inner: Arc::clone(&self.inner),
            context: Arc::clone(context),
        }))
    }
}

/// Withholds entries whose measured trust fell below the threshold.
pub(crate) struct DormantEntries;

impl HarnessRenderFilter for DormantEntries {
    fn withholds(&self, entry: &HarnessEntry) -> bool {
        is_dormant_trust(normalize_entry_trust(entry.extensions.get(TRUST_KEY)).as_ref())
    }

    fn withheld_line(&self, kind: &str, count: usize) -> String {
        format!(
            "- +{count} dormant {kind} entries (below trust threshold; still readable and editable)"
        )
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

/// What a refine is charged (TS `_gateRecurringFailures`): the
/// fingerprints recurring in `ledger` that queued it, and, unless it is a
/// failure refine held to its triggers, those that also recur in the
/// session's own ledger and were last seen within the window behind
/// `turn`. A trigger that does not recur in `ledger` is charged on the
/// session's own record (one counted while the global ledger was off).
fn gate_recurring_failures(
    ledger: &FailureLedger,
    session_ledger: &FailureLedger,
    turn: u64,
    request: Option<&FailureRequest>,
) -> Vec<FailureRecord> {
    let local = recurring_failures(session_ledger, None);
    let triggers: &[String] = request.map_or(&[], |request| &request.trigger_fingerprint_ids);
    let mut charged: Vec<String> = triggers.to_vec();
    if request.is_none_or(|request| request.kind != RequestKind::Failure) {
        charged.extend(
            local
                .iter()
                .filter(|record| {
                    turn.checked_sub(record.last_seen_turn)
                        .is_some_and(|age| age <= DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS)
                })
                .map(|record| record.fingerprint.id.clone()),
        );
    }
    let mut gated: Vec<FailureRecord> = recurring_failures(ledger, None)
        .into_iter()
        .filter(|record| charged.contains(&record.fingerprint.id))
        .collect();
    let extra: Vec<FailureRecord> = local
        .into_iter()
        .filter(|record| {
            triggers.contains(&record.fingerprint.id)
                && !gated
                    .iter()
                    .any(|charged| charged.fingerprint.id == record.fingerprint.id)
        })
        .collect();
    gated.extend(extra);
    gated
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
        request: Option<&FailureRequest>,
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
        let recurring = gate_recurring_failures(&charged_against, &session_ledger, turn, request);
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

    /// A global refine writes the state every session's ledger flush
    /// writes under the harness state lock, so it takes the same lock (TS
    /// `withHarnessStateLock` around the global apply); a local store has
    /// one writer besides the unlocked kernel, as in TS.
    fn lock_store(
        &self,
        scope: HarnessScope,
        harness_state_dir: &std::path::Path,
    ) -> anyhow::Result<Option<RefineGuard>> {
        match scope {
            HarnessScope::Global => Ok(Some(Box::new(acquire_harness_state_lock(
                harness_state_dir,
            )?))),
            HarnessScope::Local => Ok(None),
        }
    }

    fn attach_refine_requester(&self, requester: RefineRequester) {
        let inner = Arc::downgrade(&self.inner);
        let session_id = self.context.session_id.clone();
        requester.on_dropped(Arc::new(move |dropped: &PendingRefine| {
            if let Some(inner) = inner.upgrade() {
                inner.release_dropped(&session_id, dropped);
            }
        }));
        lock(&self.inner.requesters).insert(self.context.session_id.clone(), requester);
    }

    fn evaluate(
        &self,
        request: RefinementGateRequest,
    ) -> FeatureFuture<anyhow::Result<Option<Box<dyn RefinementGateVerdict>>>> {
        if !self.inner.enabled() {
            return Box::pin(async { Ok(None) });
        }
        let failure_request = request.trigger.as_ref().and_then(read_trigger);
        let (reason, kind) = failure_request.as_ref().map_or_else(
            || {
                let reason = refine_reason(request.source);
                (reason, reason.kind())
            },
            |failure| (failure.reason, failure.kind.refine_kind()),
        );
        let triggers = failure_request
            .as_ref()
            .map(|failure| failure.trigger_fingerprint_ids.clone())
            .unwrap_or_default();
        let live = LiveTriggers::hold(&self.inner, &self.context.session_id, &triggers);
        let (recurring, turn, clock) = self.charges(
            request.scope,
            &request.baseline_state,
            &request.planning_state,
            failure_request.as_ref(),
        );
        let inner = Arc::clone(&self.inner);
        let telemetry = self.context.telemetry.clone();
        let session_id = self.context.session_id.clone();
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
                            render_filters: HarnessRenderFilters(vec![Arc::new(DormantEntries)]),
                            ..HarnessStatePromptOptions::default()
                        },
                    ),
                    baseline: refinement_baseline_view(&request.baseline_state),
                    recurring_failures: &recurring,
                    turn,
                    turn_clock: clock,
                    refine_kind: kind,
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
                triggers,
                telemetry,
                rejection: Mutex::new(None),
                session_id,
                inner,
                applying: Mutex::new(None),
                _live: live,
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
    /// The failures whose recurrence or regression queued the refine.
    triggers: Vec<String>,
    telemetry: Option<FeatureTelemetry>,
    rejection: Mutex<Option<Rejection>>,
    session_id: String,
    inner: Arc<Inner>,
    /// What `prepare_application` settled, for `record_application`.
    applying: Mutex<Option<Applying>>,
    /// Marks the refine's failures live until the refine ends.
    _live: LiveTriggers,
}

/// The failures a running refine carries, live while it is held.
struct LiveTriggers {
    inner: Arc<Inner>,
    session_id: String,
    ids: Vec<String>,
}

impl LiveTriggers {
    fn hold(inner: &Arc<Inner>, session_id: &str, ids: &[String]) -> Self {
        if !ids.is_empty() {
            lock(&inner.live_triggers)
                .entry(session_id.to_string())
                .or_default()
                .push(ids.to_vec());
        }
        Self {
            inner: Arc::clone(inner),
            session_id: session_id.to_string(),
            ids: ids.to_vec(),
        }
    }
}

impl Drop for LiveTriggers {
    fn drop(&mut self) {
        if self.ids.is_empty() {
            return;
        }
        let mut live = lock(&self.inner.live_triggers);
        if let Some(refines) = live.get_mut(&self.session_id) {
            if let Some(index) = refines.iter().position(|ids| *ids == self.ids) {
                refines.remove(index);
            }
            if refines.is_empty() {
                live.remove(&self.session_id);
            }
        }
    }
}

/// The trust half of one admitted refine, between preparing the store and
/// recording the application.
struct Applying {
    /// The durable observation ordinal the commit is measured from.
    observation_turn: u64,
    settlement: TrustSettlement,
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
        if !self.triggers.is_empty() {
            extensions.insert(
                "triggerFingerprintIds".to_string(),
                Value::from(self.triggers.clone()),
            );
        }
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

    /// Trust is settled and claimed on the durable observation ordinal.
    /// Settling first means this commit's own claim cannot be credited by
    /// the window it is about to open; evidence a skipped flush left
    /// pending is folded in first, so a recurred window never closes clean
    /// (it stays pending: recording it again is a no-op).
    fn prepare_application(&self, state: &mut HarnessState) {
        let observation_turn = self.inner.fresh_global_ordinal(&self.session_id);
        let evidence = self.inner.trust.pending(&self.session_id, self.scope);
        let windows =
            record_harness_trust_evidence(stored_trust_windows(state).as_ref(), state, &evidence);
        let mut settlement = TrustSettlement::default();
        if let Some(windows) = windows {
            let (windows, settled) =
                settle_harness_trust(&windows, state, observation_turn, &pa_ledger::now_iso());
            settlement = settled;
            state
                .extensions
                .insert(TRUST_WINDOWS_KEY.to_string(), trust_windows_value(&windows));
        }
        *lock(&self.applying) = Some(Applying {
            observation_turn,
            settlement,
        });
    }

    fn record_application(&self, state: &mut HarnessState, result: &mut RefinementResult) {
        let all_applied = result.applied_edits.iter().all(|edit| edit.applied);
        self.record_trust(state, result, all_applied);
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
        if !self.triggers.is_empty() {
            result.extensions.insert(
                "triggerFingerprintIds".to_string(),
                Value::from(self.triggers.clone()),
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

impl RavoVerdict {
    /// Every entry the commit wrote carries a trust record from now on,
    /// and a commit that claimed fingerprints opens a trust window over
    /// the entries it wrote (TS `applyRefinementProposal`'s `trustClaim`).
    fn record_trust(
        &self,
        state: &mut HarnessState,
        result: &mut RefinementResult,
        all_applied: bool,
    ) {
        let applying = lock(&self.applying).take();
        let mut touched = Vec::new();
        let mut skill_imports = indexmap::IndexMap::new();
        for edit in &mut result.applied_edits {
            if edit.action == RefinementAction::Delete {
                continue;
            }
            let Some(after) = edit.after.as_mut() else {
                continue;
            };
            if !after.extensions.contains_key(TRUST_KEY) {
                let trust = serde_json::to_value(empty_entry_trust(&after.updated_at))
                    .unwrap_or(Value::Null);
                after.extensions.insert(TRUST_KEY.to_string(), trust);
            }
            if !(all_applied && edit.applied) {
                continue;
            }
            if let Some(stored) = state
                .entries
                .get_mut(&edit.kind)
                .and_then(|records| records.get_mut(&edit.id))
            {
                if let Some(trust) = after.extensions.get(TRUST_KEY) {
                    stored
                        .extensions
                        .entry(TRUST_KEY.to_string())
                        .or_insert_with(|| trust.clone());
                }
            }
            let entry_ref = harness_entry_ref(kind_str(edit.kind), &edit.id);
            if edit.kind == RefinementKind::Skill {
                let imports = reference_imports(&after.reference);
                if imports.is_empty() {
                    skill_imports.shift_remove(&entry_ref);
                } else {
                    skill_imports.insert(entry_ref.clone(), imports);
                }
            }
            touched.push(entry_ref);
        }
        let Some(applying) = applying else {
            return;
        };
        let claimed = &self.report.addressed_fingerprints;
        if all_applied && !claimed.is_empty() && !touched.is_empty() {
            let windows = open_trust_window(
                stored_trust_windows(state).as_ref(),
                &TrustClaim {
                    proposal_id: self.proposal_id.clone(),
                    touched,
                    claimed_fingerprints: claimed.clone(),
                    committed_turn: applying.observation_turn,
                    until_turn: applying.observation_turn + DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS,
                    skill_imports,
                },
            );
            state
                .extensions
                .insert(TRUST_WINDOWS_KEY.to_string(), trust_windows_value(&windows));
        }
        log_trust_settlement(&applying.settlement, scope_name(self.scope));
    }
}

fn kind_str(kind: RefinementKind) -> &'static str {
    match kind {
        RefinementKind::Prompt => "prompt",
        RefinementKind::Memory => "memory",
        RefinementKind::Skill => "skill",
        RefinementKind::Subagent => "subagent",
        RefinementKind::Factory => "factory",
    }
}

/// The ledger observer half.
struct RavoLedgerObserver {
    inner: Arc<Inner>,
}

impl RavoLedgerObserver {
    /// Find the provisional champions a boundary's recurrences regressed,
    /// each window on the clock it was stamped with, and queue them for the
    /// flush of their scope; answers (local, global) regressions.
    fn find_regressions(
        &self,
        context: &SessionFeatureContext,
        boundary: &LedgerBoundary<'_>,
        local: Option<&HarnessDocument>,
    ) -> (Vec<ProvisionalRegression>, Vec<ProvisionalRegression>) {
        let Some(local) = local else {
            return (Vec::new(), Vec::new());
        };
        let local_ravo = local.get(RAVO_KEY);
        let on_local_clock = find_provisional_regressions(
            local_ravo,
            boundary.recurred_ids,
            boundary.local_ordinal,
            RavoWindowClock::LocalOrdinal.as_str(),
        );
        let on_global_clock = boundary
            .global_ordinal
            .map(|ordinal| {
                find_provisional_regressions(
                    local_ravo,
                    boundary.recurred_ids,
                    ordinal,
                    RavoWindowClock::Ordinal.as_str(),
                )
            })
            .unwrap_or_default();
        let global = boundary
            .global_ordinal
            .zip(boundary.global_state)
            .map(|(ordinal, document)| {
                find_provisional_regressions(
                    document.get(RAVO_KEY),
                    boundary.recurred_ids,
                    ordinal,
                    RavoWindowClock::Ordinal.as_str(),
                )
            })
            .unwrap_or_default();
        let mut pending = lock(&self.inner.regressions);
        let pending = pending.entry(context.session_id.clone()).or_default();
        if !on_local_clock.is_empty() {
            pending
                .local
                .push((on_local_clock.clone(), boundary.local_ordinal));
        }
        if let (false, Some(ordinal)) = (on_global_clock.is_empty(), boundary.global_ordinal) {
            pending.local.push((on_global_clock.clone(), ordinal));
        }
        if let (false, Some(ordinal)) = (global.is_empty(), boundary.global_ordinal) {
            pending.global.push((global.clone(), ordinal));
        }
        let mut local = on_local_clock;
        local.extend(on_global_clock);
        (local, global)
    }

    /// A parked request runs once the one ahead of it is gone.
    fn release_parked(&self, session_id: &str, requester: &RefineRequester) {
        let mut parked = lock(&self.inner.parked);
        let Some(queued) = parked
            .get_mut(session_id)
            .filter(|queued| !queued.is_empty())
        else {
            return;
        };
        requester.update(|pending| match pending {
            Some(pending) => Some(pending),
            None => Some(queued.remove(0)),
        });
    }

    fn enqueue(&self, session_id: &str, requester: &RefineRequester, request: PendingRefine) {
        let mut parked = lock(&self.inner.parked);
        let parked = parked.entry(session_id.to_string()).or_default();
        requester.update(|pending| Some(queue(pending, request, parked)));
    }

    /// Queue the refines a boundary triggers (TS
    /// `_observeFailuresAtTurnBoundary`): a regression repair per scope of
    /// the regressed champions, else a recurrence refine for the
    /// fingerprints that entered the recurring set; each fingerprint
    /// triggers each kind once per session.
    fn queue_failure_refines(
        &self,
        session_id: &str,
        requester: &RefineRequester,
        boundary: &LedgerBoundary<'_>,
        local: Vec<ProvisionalRegression>,
        global: Vec<ProvisionalRegression>,
    ) {
        let mut triggered = lock(&self.inner.triggered);
        let triggered = triggered.entry(session_id.to_string()).or_default();
        let untriggered = |regressions: Vec<ProvisionalRegression>| -> Vec<ProvisionalRegression> {
            regressions
                .into_iter()
                .filter(|regression| {
                    regression
                        .fingerprints
                        .iter()
                        .any(|id| !triggered.contains(&format!("regression:{id}")))
                })
                .collect()
        };
        let repairs: Vec<(Vec<ProvisionalRegression>, bool)> =
            [(untriggered(local), false), (untriggered(global), true)]
                .into_iter()
                .filter(|(regressed, _)| !regressed.is_empty())
                .collect();
        if !repairs.is_empty() {
            for (regressed, _) in &repairs {
                for id in regressed
                    .iter()
                    .flat_map(|regression| &regression.fingerprints)
                {
                    triggered.insert(format!("regression:{id}"));
                }
            }
            for (regressed, global) in repairs {
                let mut ids: Vec<String> = Vec::new();
                for id in regressed
                    .iter()
                    .flat_map(|regression| &regression.fingerprints)
                {
                    if !ids.contains(id) {
                        ids.push(id.clone());
                    }
                }
                let records: Vec<FailureRecord> = boundary
                    .effective
                    .failures
                    .values()
                    .filter(|record| ids.contains(&record.fingerprint.id))
                    .cloned()
                    .collect();
                let instructions = format_regression_refine_instructions(&regressed, &records);
                self.enqueue(
                    session_id,
                    requester,
                    failure_refine(instructions, RefineReason::Regression, ids, global),
                );
            }
            return;
        }
        let recurring: Vec<FailureRecord> = boundary
            .newly_recurring
            .iter()
            .filter(|record| !triggered.contains(&format!("recurrence:{}", record.fingerprint.id)))
            .cloned()
            .collect();
        if recurring.is_empty() {
            return;
        }
        for record in &recurring {
            triggered.insert(format!("recurrence:{}", record.fingerprint.id));
        }
        let ids = recurring
            .iter()
            .map(|record| record.fingerprint.id.clone())
            .collect();
        self.enqueue(
            session_id,
            requester,
            failure_refine(
                format_recurrence_refine_instructions(&recurring),
                RefineReason::Recurrence,
                ids,
                false,
            ),
        );
    }
}

impl LedgerObserver for RavoLedgerObserver {
    fn on_boundary(&self, context: &Arc<SessionFeatureContext>, boundary: &LedgerBoundary<'_>) {
        // The self-checks are queued before the trust replays are planned
        // (a replay awaits a check still queued) and start after.
        let start_checks = self.inner.ledger.get().is_some()
            && self.inner.verifier.enqueue(
                &context.session_id,
                boundary.observations,
                boundary.effective,
            );
        let requester = lock(&self.inner.requesters)
            .get(&context.session_id)
            .cloned();
        if let Some(requester) = &requester {
            self.release_parked(&context.session_id, requester);
        }
        self.inner.remember(context);
        // One read of the local state serves the regressions and the trust
        // windows; only when something recurred.
        let local_document = context
            .session_artifact_dir
            .as_deref()
            .filter(|_| !boundary.recurred_ids.is_empty())
            .map(|artifact_dir| HarnessDocument::load(&local_harness_state_dir(artifact_dir)));
        let (local, global) = self.find_regressions(context, boundary, local_document.as_ref());
        if let Some(run) = self.inner.trust_runner() {
            self.inner.trust.observe_recurrences(
                &context.session_id,
                boundary,
                local_document.as_ref(),
                &self.inner.verifier,
                &run,
            );
        }
        if let (true, Some(handle)) = (start_checks, self.inner.ledger.get()) {
            self.inner.verifier.spawn(
                &context.session_id,
                Arc::clone(&self.inner.options.runner),
                handle.clone(),
            );
        }
        if let Some(requester) = &requester {
            self.queue_failure_refines(&context.session_id, requester, boundary, local, global);
        }
    }

    fn hold_flush(&self, session_id: &str) -> bool {
        lock(&self.inner.refines).contains_key(session_id)
    }

    fn wants_local_flush(&self, session_id: &str) -> bool {
        self.inner.trust.wants_flush(session_id, LedgerScope::Local)
    }

    fn wants_global_flush(&self, session_id: &str) -> bool {
        lock(&self.inner.regressions)
            .get(session_id)
            .is_some_and(|pending| !pending.global.is_empty())
            || self
                .inner
                .trust
                .wants_flush(session_id, LedgerScope::Global)
    }

    fn on_flush(&self, flush: &mut LedgerFlush<'_>) {
        let session_id = flush.session_id.to_string();
        self.inner
            .trust
            .on_flush(flush, || self.inner.fresh_global_ordinal(&session_id));
        self.record_regressions(flush);
    }

    fn on_flush_result(&self, scope: LedgerScope, session_id: &str, landed: bool) {
        self.inner.trust.on_flush_result(scope, session_id, landed);
        self.regressions_flushed(scope, session_id, landed);
    }
}

impl RavoLedgerObserver {
    fn record_regressions(&self, flush: &mut LedgerFlush<'_>) {
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

    fn regressions_flushed(&self, scope: LedgerScope, session_id: &str, landed: bool) {
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

#[cfg(test)]
mod tests {
    use pa_ledger::{fingerprint_failure, FailureKind, FailureLedger, FailureRecord};

    use super::*;

    fn record(seed: &str, count: u64, last_seen_turn: u64) -> FailureRecord {
        FailureRecord {
            fingerprint: fingerprint_failure(FailureKind::ToolError, Some("bash"), None, seed),
            count,
            first_seen_turn: 1,
            last_seen_turn,
            first_seen_at: String::new(),
            last_seen_at: String::new(),
            excerpt: format!("{seed}: exit 1"),
            addressed_by_proposal_ids: Vec::new(),
            replay_cases: Vec::new(),
            non_actionable_count: None,
        }
    }

    fn ledger(records: &[&FailureRecord]) -> FailureLedger {
        FailureLedger {
            failures: records
                .iter()
                .map(|record| (record.fingerprint.id.clone(), (*record).clone()))
                .collect(),
            ..FailureLedger::default()
        }
    }

    fn ids(records: &[FailureRecord]) -> Vec<&str> {
        records
            .iter()
            .map(|record| record.fingerprint.id.as_str())
            .collect()
    }

    /// TS `_gateRecurringFailures`: a failure refine is held to its
    /// triggers, charged on the session's own record when the ledger it
    /// is judged on does not have it recurring; any other refine is also
    /// charged what recurs in both ledgers and was seen recently on this
    /// branch.
    #[test]
    fn a_refine_is_charged_its_triggers_and_recent_recurrences() {
        let recent = record("recent", 3, 30);
        let stale = record("stale", 3, 5);
        let ahead = record("ahead", 3, 45);
        let local_only = record("local only", 2, 30);
        let judged_on = ledger(&[&recent, &stale, &ahead]);
        let session = ledger(&[&recent, &stale, &ahead, &local_only]);
        let triggered = |kind: RequestKind, ids: &[&FailureRecord]| FailureRequest {
            reason: RefineReason::Recurrence,
            kind,
            trigger_fingerprint_ids: ids
                .iter()
                .map(|record| record.fingerprint.id.clone())
                .collect(),
        };
        // Directed, no trigger: recency only (age 0..=20 behind turn 40).
        assert_eq!(
            ids(&gate_recurring_failures(&judged_on, &session, 40, None)),
            [recent.fingerprint.id.as_str()]
        );
        // A failure refine: its triggers only, the local-only one from the
        // session's own ledger.
        let failure = triggered(RequestKind::Failure, &[&stale, &local_only]);
        assert_eq!(
            ids(&gate_recurring_failures(
                &judged_on,
                &session,
                40,
                Some(&failure)
            )),
            [
                stale.fingerprint.id.as_str(),
                local_only.fingerprint.id.as_str()
            ]
        );
        // Joined by the agent (directed): triggers plus recency.
        let directed = triggered(RequestKind::Directed, &[&stale]);
        assert_eq!(
            ids(&gate_recurring_failures(
                &judged_on,
                &session,
                40,
                Some(&directed)
            )),
            [
                recent.fingerprint.id.as_str(),
                stale.fingerprint.id.as_str()
            ]
        );
    }
}
