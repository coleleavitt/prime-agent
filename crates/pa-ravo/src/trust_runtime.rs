//! The session side of trust (TS `_observeTrustWindowRecurrences`,
//! `_releaseTrustAdjudications`, `_drainTrustAdjudicationBacklog` and the
//! trust half of `_flushFailureLedger` / `_flushGlobalFailureLedger`):
//! recurrences inside open windows become evidence, the replays they
//! warrant run off the turn path, and the evidence is recorded and the
//! windows settled when the ledger flushes the store they belong to.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use indexmap::IndexMap;
use pa_core::refinement::HarnessScope;
use pa_ledger::{
    FailureLedger,
    FailureRecord,
    HarnessDocument,
    LedgerBoundary,
    LedgerFlush,
    LedgerHandle,
    LedgerScope,
    ReplayCase,
    ReplayVerification,
    apply_replay_verifications,
    observation_ordinal,
};
use pa_types::trace_context::SPAN_ATTRIBUTES_TARGET;
use serde_json::{Map, Value};

use crate::referee::ReplayRunner;
use crate::trust::{
    TRUST_WINDOWS_KEY,
    TrustOutcome,
    TrustSettlement,
    TrustWindowEvidence,
    has_open_trust_windows,
    log_trust_settlement,
    normalize_trust_windows,
    record_harness_trust_evidence,
    settle_harness_trust,
    trust_windows_value,
};
use crate::trust_adjudication::{
    AwaitingTrustAdjudication,
    MAX_TRUST_ADJUDICATION_JOBS,
    TrustAdjudicationJob,
    TrustAdjudicationRun,
    TrustPlanInput,
    adjudicate_trust_recurrences,
    find_trust_window_recurrences,
    plan_trust_adjudications,
    release_awaiting_trust_adjudication,
};
use crate::verification::ReplayVerifier;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One session's trust bookkeeping.
#[derive(Default)]
struct SessionTrust {
    /// Evidence waiting for the next flush (or apply) of each store.
    local: Vec<TrustWindowEvidence>,
    global: Vec<TrustWindowEvidence>,
    /// How much of each list the announced flush recorded, and what it
    /// settled (logged once the write landed).
    local_recorded: usize,
    global_recorded: usize,
    local_settlement: Option<TrustSettlement>,
    global_settlement: Option<TrustSettlement>,
    /// Planned replays not yet running.
    backlog: Vec<TrustAdjudicationJob>,
    /// Keys of the jobs backlogged, running or awaiting.
    queued: HashSet<String>,
    awaiting: IndexMap<String, AwaitingTrustAdjudication>,
    running: bool,
}

impl SessionTrust {
    fn pending(&mut self, scope: LedgerScope) -> &mut Vec<TrustWindowEvidence> {
        match scope {
            LedgerScope::Local => &mut self.local,
            LedgerScope::Global => &mut self.global,
        }
    }
}

/// What a replay batch runs with.
#[derive(Clone)]
pub(crate) struct TrustRunner {
    pub runner: Arc<dyn ReplayRunner>,
    pub sys_path: Vec<String>,
    pub handle: LedgerHandle,
}

/// Every session's trust bookkeeping.
#[derive(Default)]
pub(crate) struct TrustTracker {
    sessions: Mutex<HashMap<String, SessionTrust>>,
    /// Notified whenever a session's replay batches stop.
    stopped: Condvar,
}

fn scope_of(scope: LedgerScope) -> HarnessScope {
    match scope {
        LedgerScope::Local => HarnessScope::Local,
        LedgerScope::Global => HarnessScope::Global,
    }
}

fn ledger_scope(scope: HarnessScope) -> LedgerScope {
    match scope {
        HarnessScope::Local => LedgerScope::Local,
        HarnessScope::Global => LedgerScope::Global,
    }
}

fn scope_name(scope: LedgerScope) -> &'static str {
    match scope {
        LedgerScope::Local => "local",
        LedgerScope::Global => "global",
    }
}

/// `record` with `verifications` applied.
fn with_verifications(
    record: &FailureRecord,
    verifications: &[ReplayVerification],
) -> FailureRecord {
    let mut single = FailureLedger::default();
    single
        .failures
        .insert(record.fingerprint.id.clone(), record.clone());
    apply_replay_verifications(&single, verifications)
        .failures
        .shift_remove(&record.fingerprint.id)
        .unwrap_or_else(|| record.clone())
}

impl TrustTracker {
    /// The evidence waiting for `scope`'s next write.
    pub(crate) fn pending(
        &self,
        session_id: &str,
        scope: HarnessScope,
    ) -> Vec<TrustWindowEvidence> {
        lock(&self.sessions)
            .get(session_id)
            .map(|session| match scope {
                HarnessScope::Local => session.local.clone(),
                HarnessScope::Global => session.global.clone(),
            })
            .unwrap_or_default()
    }

    pub(crate) fn wants_flush(&self, session_id: &str, scope: LedgerScope) -> bool {
        lock(&self.sessions)
            .get_mut(session_id)
            .is_some_and(|session| !session.pending(scope).is_empty())
    }

    /// Record where a committed refinement's claimed failure recurred
    /// inside its window, and plan the replays that warrants, in the local
    /// and then the global store. Only with the global ledger on (windows
    /// are measured on its ordinal) and something recurring.
    pub(crate) fn observe_recurrences(
        self: &Arc<Self>,
        session_id: &str,
        boundary: &LedgerBoundary<'_>,
        local: Option<&HarnessDocument>,
        verifier: &ReplayVerifier,
        run: &TrustRunner,
    ) {
        let Some(ordinal) = boundary.global_ordinal else {
            return;
        };
        if boundary.recurred_ids.is_empty() {
            return;
        }
        let mut recurred: IndexMap<String, Vec<ReplayCase>> = IndexMap::new();
        for observation in boundary.observations {
            let id = &observation.fingerprint.id;
            if !boundary.recurred_ids.contains(id) || !observation.is_actionable() {
                continue;
            }
            let cases = recurred.entry(id.clone()).or_default();
            if let Some(case) = &observation.replay_case {
                cases.push(case.clone());
            }
        }
        let verifications = run.handle.pending_replay_verifications(session_id);
        let record_of = |id: &str| {
            boundary
                .effective
                .failures
                .get(id)
                .or_else(|| boundary.local.failures.get(id))
                .map(|record| with_verifications(record, &verifications))
        };
        let mut sessions = lock(&self.sessions);
        let session = sessions.entry(session_id.to_string()).or_default();
        for (scope, document) in [
            (LedgerScope::Local, local),
            (LedgerScope::Global, boundary.global_state),
        ] {
            let Some(document) = document else {
                continue;
            };
            let entries = document
                .get("entries")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let stored = normalize_trust_windows(document.get(TRUST_WINDOWS_KEY));
            let windows =
                record_harness_trust_evidence(stored.as_ref(), &entries, session.pending(scope));
            let recurrences = find_trust_window_recurrences(windows.as_ref(), &recurred, ordinal);
            if recurrences.is_empty() {
                continue;
            }
            let evidence: Vec<TrustWindowEvidence> = recurrences
                .iter()
                .map(|recurrence| TrustWindowEvidence::Recurrence {
                    proposal_id: recurrence.proposal_id.clone(),
                    fingerprint_id: recurrence.fingerprint_id.clone(),
                    ordinal,
                })
                .collect();
            session.pending(scope).extend(evidence.iter().cloned());
            let windows = record_harness_trust_evidence(windows.as_ref(), &entries, &evidence);
            let (jobs, awaiting) = plan_trust_adjudications(&TrustPlanInput {
                scope: scope_of(scope),
                windows: windows.as_ref(),
                recurrences: &recurrences,
                entries: &entries,
                record_of: &record_of,
            });
            for job in jobs {
                let key = job.key();
                if session.queued.contains(&key) {
                    continue;
                }
                if session.backlog.len() >= MAX_TRUST_ADJUDICATION_JOBS {
                    break;
                }
                session.backlog.push(job);
                session.queued.insert(key);
            }
            for awaiting in awaiting {
                let key = awaiting.job.key();
                if session.queued.contains(&key) {
                    continue;
                }
                if session.awaiting.len() >= MAX_TRUST_ADJUDICATION_JOBS {
                    break;
                }
                if !verifier.is_pending(
                    session_id,
                    &awaiting.job.fingerprint_id,
                    &awaiting.sources,
                    true,
                ) {
                    continue;
                }
                session.awaiting.insert(key.clone(), awaiting);
                session.queued.insert(key);
            }
        }
        drop(sessions);
        self.drain(session_id, run);
    }

    /// Hand a finished self-check batch's verifications to the replays
    /// awaiting them. A released job joins the backlog while it has room;
    /// one whose sources no later batch will check is dropped (and may be
    /// planned again).
    pub(crate) fn release(
        self: &Arc<Self>,
        session_id: &str,
        verifications: &[ReplayVerification],
        verifier: &ReplayVerifier,
        run: &TrustRunner,
    ) {
        {
            let mut sessions = lock(&self.sessions);
            let Some(session) = sessions.get_mut(session_id) else {
                return;
            };
            let keys: Vec<String> = session.awaiting.keys().cloned().collect();
            for key in keys {
                let awaiting = &session.awaiting[&key];
                let released = release_awaiting_trust_adjudication(awaiting, verifications);
                if !released.matched
                    && verifier.is_pending(
                        session_id,
                        &awaiting.job.fingerprint_id,
                        &awaiting.sources,
                        false,
                    )
                {
                    continue;
                }
                session.awaiting.shift_remove(&key);
                match released.job {
                    Some(job) if session.backlog.len() < MAX_TRUST_ADJUDICATION_JOBS => {
                        session.backlog.push(job);
                    }
                    _ => {
                        session.queued.remove(&key);
                    }
                }
            }
        }
        self.drain(session_id, run);
    }

    /// Run the session's backlogged replays on a thread of their own, one
    /// batch at a time.
    fn drain(self: &Arc<Self>, session_id: &str, run: &TrustRunner) {
        {
            let mut sessions = lock(&self.sessions);
            let Some(session) = sessions.get_mut(session_id) else {
                return;
            };
            if session.running || session.backlog.is_empty() {
                return;
            }
            session.running = true;
        }
        let tracker = Arc::clone(self);
        let owned = session_id.to_string();
        let run = run.clone();
        let spawned = std::thread::Builder::new()
            .name("ravo-trust-replay".to_string())
            .spawn(move || tracker.run_batches(&owned, &run));
        if let Err(error) = spawned {
            tracing::warn!(%error, "trust replays could not start; no trust moves");
            self.stop(session_id, true);
        }
    }

    fn stop(&self, session_id: &str, clear: bool) {
        if let Some(session) = lock(&self.sessions).get_mut(session_id) {
            session.running = false;
            if clear {
                for job in session.backlog.drain(..) {
                    session.queued.remove(&job.key());
                }
            }
        }
        self.stopped.notify_all();
    }

    fn run_batches(&self, session_id: &str, run: &TrustRunner) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        let Ok(runtime) = runtime else {
            // A replay that cannot run records no verdict, which never
            // moves trust.
            self.stop(session_id, true);
            return;
        };
        let aborted = AtomicBool::new(false);
        loop {
            let batch = {
                let mut sessions = lock(&self.sessions);
                match sessions.get_mut(session_id) {
                    Some(session) if !session.backlog.is_empty() => {
                        std::mem::take(&mut session.backlog)
                    }
                    _ => {
                        drop(sessions);
                        self.stop(session_id, false);
                        return;
                    }
                }
            };
            let evidence = runtime.block_on(adjudicate_trust_recurrences(
                &batch,
                TrustAdjudicationRun {
                    runner: run.runner.as_ref(),
                    sys_path: &run.sys_path,
                    session_id,
                    aborted: &aborted,
                    now: &pa_ledger::now_iso,
                },
            ));
            let mut global = false;
            {
                let mut sessions = lock(&self.sessions);
                let session = sessions.entry(session_id.to_string()).or_default();
                for item in evidence {
                    let scope = ledger_scope(item.scope);
                    global |= scope == LedgerScope::Global;
                    session.pending(scope).push(item.evidence);
                }
                for job in &batch {
                    session.queued.remove(&job.key());
                }
            }
            // Global verdicts are flushed now; local ones wait for the next
            // local flush (the kernel writes the local state without a lock,
            // and a batch usually lands while a cell runs).
            if global {
                run.handle.request_global_flush(session_id);
            }
        }
    }

    /// Wait until no session's replays run, up to `deadline`.
    pub(crate) fn wait_idle(&self, deadline: Instant) -> bool {
        let mut sessions = lock(&self.sessions);
        while sessions.values().any(|session| session.running) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            sessions = self
                .stopped
                .wait_timeout(sessions, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Record the scope's pending evidence into the document being written
    /// and settle its windows: the local store whenever a window is open or
    /// evidence waits (on `global_ordinal`, the clock windows are measured
    /// on), the global one whenever it has windows (on the ordinal of the
    /// ledger it is writing).
    pub(crate) fn on_flush(
        &self,
        flush: &mut LedgerFlush<'_>,
        global_ordinal: impl FnOnce() -> u64,
    ) {
        let scope = flush.scope;
        let evidence = {
            let mut sessions = lock(&self.sessions);
            let session = sessions.entry(flush.session_id.to_string()).or_default();
            let evidence = session.pending(scope).clone();
            match scope {
                LedgerScope::Local => {
                    session.local_recorded = evidence.len();
                    session.local_settlement = None;
                }
                LedgerScope::Global => {
                    session.global_recorded = evidence.len();
                    session.global_settlement = None;
                }
            }
            evidence
        };
        if scope == LedgerScope::Global {
            let adjudications = evidence
                .iter()
                .filter(|item| item.is_adjudication())
                .count() as u64;
            tracing::event!(
                target: SPAN_ATTRIBUTES_TARGET,
                tracing::Level::INFO,
                trust.recurrences = evidence.len() as u64 - adjudications,
                trust.adjudications = adjudications,
            );
        }
        let stored = normalize_trust_windows(flush.document.get(TRUST_WINDOWS_KEY));
        let mut entries: Map<String, Value> = flush
            .document
            .get("entries")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let windows = record_harness_trust_evidence(stored.as_ref(), &entries, &evidence);
        let settle = match scope {
            LedgerScope::Local => has_open_trust_windows(windows.as_ref()) || !evidence.is_empty(),
            LedgerScope::Global => windows.is_some(),
        };
        let mut settlement = None;
        let windows = match windows {
            Some(windows) if settle => {
                let turn = match scope {
                    LedgerScope::Local => global_ordinal(),
                    LedgerScope::Global => observation_ordinal(Some(&flush.document.failures())),
                };
                let (windows, settled) =
                    settle_harness_trust(&windows, &mut entries, turn, &pa_ledger::now_iso());
                if !settled.adjustments.is_empty() {
                    flush.document.set("entries", Value::Object(entries));
                }
                settlement = Some(settled);
                Some(windows)
            }
            other => other,
        };
        if let Some(windows) = &windows {
            flush
                .document
                .set(TRUST_WINDOWS_KEY, trust_windows_value(windows));
        }
        if scope == LedgerScope::Global {
            let count = |outcome| {
                settlement
                    .as_ref()
                    .map_or(0, |settled: &TrustSettlement| settled.count(outcome))
            };
            tracing::event!(
                target: SPAN_ATTRIBUTES_TARGET,
                tracing::Level::INFO,
                trust.faulted = count(TrustOutcome::Faulted),
                trust.clean = count(TrustOutcome::Clean),
                trust.contested = count(TrustOutcome::Contested),
            );
        }
        let mut sessions = lock(&self.sessions);
        if let Some(session) = sessions.get_mut(flush.session_id) {
            match scope {
                LedgerScope::Local => session.local_settlement = settlement,
                LedgerScope::Global => session.global_settlement = settlement,
            }
        }
    }

    /// Drop what the announced flush recorded once it landed, and log what
    /// it settled.
    pub(crate) fn on_flush_result(&self, scope: LedgerScope, session_id: &str, landed: bool) {
        let settlement = {
            let mut sessions = lock(&self.sessions);
            let Some(session) = sessions.get_mut(session_id) else {
                return;
            };
            let (recorded, settlement) = match scope {
                LedgerScope::Local => (
                    std::mem::take(&mut session.local_recorded),
                    session.local_settlement.take(),
                ),
                LedgerScope::Global => (
                    std::mem::take(&mut session.global_recorded),
                    session.global_settlement.take(),
                ),
            };
            if !landed {
                return;
            }
            let pending = session.pending(scope);
            pending.drain(..recorded.min(pending.len()));
            settlement
        };
        if let Some(settlement) = settlement {
            log_trust_settlement(&settlement, scope_name(scope));
        }
    }
}
