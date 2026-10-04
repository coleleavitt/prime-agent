//! Replay self-checks (TS `_startReplayVerification` and
//! `verifyObservedReplayCases`): a replay case derived from a fresh
//! observation is evidence only once it has reproduced its recorded
//! exception. Each session's new cases are run off the turn path, one batch
//! at a time on a thread of their own, each (fingerprint, source) at most
//! once per session and never one the ledger already holds verified; the
//! reproductions wait in the ledger for its next flush, which respects the
//! hold a running refine puts on it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use pa_ledger::{FailureLedger, FailureObservation, LedgerHandle, ReplayCase, ReplayVerification};

use crate::referee::{verdict_from_outcome, RefereeVerdictStatus, ReplayEnvironment, ReplayRunner};

/// A case waiting for its self-check.
struct Pending {
    fingerprint_id: String,
    case: ReplayCase,
}

#[derive(Default)]
struct SessionChecks {
    attempted: HashSet<(String, String)>,
    backlog: Vec<Pending>,
    running: bool,
}

/// The per-session self-check queues.
#[derive(Default)]
pub(crate) struct ReplayVerifier {
    sessions: Mutex<HashMap<String, SessionChecks>>,
    /// Notified whenever a session's batches stop.
    stopped: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ReplayVerifier {
    /// Queue the unverified cases `observations` derived and start a batch
    /// when none is running. `ledger` is the ledger recurrence is judged
    /// on: a case it holds verified is not run again.
    pub(crate) fn observe(
        self: &Arc<Self>,
        session_id: &str,
        observations: &[FailureObservation],
        ledger: &FailureLedger,
        runner: Arc<dyn ReplayRunner>,
        handle: LedgerHandle,
    ) {
        let start = {
            let mut sessions = lock(&self.sessions);
            let checks = sessions.entry(session_id.to_string()).or_default();
            for observation in observations {
                let Some(case) = &observation.replay_case else {
                    continue;
                };
                if case.verified_at.is_some() {
                    continue;
                }
                let key = (observation.fingerprint.id.clone(), case.source.clone());
                if !checks.attempted.insert(key) {
                    continue;
                }
                let held = ledger
                    .failures
                    .get(&observation.fingerprint.id)
                    .is_some_and(|record| {
                        record
                            .verified_replay_cases()
                            .iter()
                            .any(|verified| verified.source == case.source)
                    });
                if held {
                    continue;
                }
                checks.backlog.push(Pending {
                    fingerprint_id: observation.fingerprint.id.clone(),
                    case: case.clone(),
                });
            }
            let start = !checks.running && !checks.backlog.is_empty();
            checks.running |= start;
            start
        };
        if !start {
            return;
        }
        let verifier = Arc::clone(self);
        let owned = session_id.to_string();
        let spawned = std::thread::Builder::new()
            .name("ravo-replay-verify".to_string())
            .spawn(move || verifier.drain(&owned, runner.as_ref(), &handle));
        if let Err(error) = spawned {
            tracing::warn!(%error, "replay self-check could not start; the cases stay unverified");
            self.stop(session_id);
        }
    }

    fn stop(&self, session_id: &str) {
        if let Some(checks) = lock(&self.sessions).get_mut(session_id) {
            checks.running = false;
        }
        self.stopped.notify_all();
    }

    /// Wait until no session's self-checks run, up to `deadline`; `false`
    /// when the deadline passed first.
    pub(crate) fn wait_idle(&self, deadline: Instant) -> bool {
        let mut sessions = lock(&self.sessions);
        while sessions.values().any(|checks| checks.running) {
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

    /// Run batches until the session's backlog is empty.
    fn drain(&self, session_id: &str, runner: &dyn ReplayRunner, handle: &LedgerHandle) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        loop {
            let batch = {
                let mut sessions = lock(&self.sessions);
                let checks = sessions
                    .get_mut(session_id)
                    .filter(|checks| !checks.backlog.is_empty());
                let Some(checks) = checks else {
                    drop(sessions);
                    self.stop(session_id);
                    return;
                };
                std::mem::take(&mut checks.backlog)
            };
            // A self-check that cannot run leaves its cases unverified, the
            // conservative state.
            let Ok(runtime) = runtime.as_ref() else {
                continue;
            };
            let verifications: Vec<ReplayVerification> = runtime.block_on(async {
                let mut verified = Vec::new();
                for pending in &batch {
                    let outcome = runner
                        .run(&pending.case, ReplayEnvironment::Sanitized, &[])
                        .await;
                    if verdict_from_outcome(&pending.case, &outcome) == RefereeVerdictStatus::Upheld
                    {
                        verified.push(ReplayVerification {
                            fingerprint_id: pending.fingerprint_id.clone(),
                            source: pending.case.source.clone(),
                            verified_at: pa_ledger::now_iso(),
                        });
                    }
                }
                verified
            });
            if !verifications.is_empty() {
                handle.record_replay_verifications(session_id, &verifications);
            }
        }
    }
}
