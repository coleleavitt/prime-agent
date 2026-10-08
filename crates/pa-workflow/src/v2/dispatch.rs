//! The at-most-once physical-dispatch guard (`WORKFLOW-V2.md` §7 "never
//! re-dispatches", §9 "never automatically replay a model turn"; TS
//! `workflow-v2-retained-executor.ts`, slice 3).
//!
//! The executor's sole authorization point for the one physical provider
//! call of an admitted turn. The physical call is reachable only right
//! after a freshly committed `dispatching` journal fact, and that fact
//! commits at most once per admitted turn (the journal claims the exact
//! materialization outbox and asserts no prior dispatching/provider-entered
//! fact in one fenced transaction). So across every attempt, restart, and
//! relaunch the provider is entered at most once; the crash gap between
//! the `dispatching` commit and the network call is terminal
//! `execution_unknown`, never a relaunch. The guard holds no state: the
//! injected [`DispatchJournal`] is the authority, so a fresh guard after a
//! restart reaches the same decision.

use std::future::Future;

/// One admitted turn whose dispatch the guard authorizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchBinding {
    pub rlm_child_id: String,
    pub turn_id: String,
    pub admission_receipt_digest: String,
}

/// The journal-proven dispatch facts for one binding.
#[allow(clippy::struct_excessive_bools)] // the journal's four independent closed facts
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DispatchFacts {
    /// The capture slot is armed and `capture_armed` committed.
    pub capture_armed: bool,
    /// An exact, unclaimed materialization outbox exists.
    pub outbox_unclaimed: bool,
    /// A `dispatching` fact is durably committed.
    pub has_dispatching: bool,
    /// A `provider_entered` fact is durably committed.
    pub has_provider_entered: bool,
}

/// Why the atomic claim was lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimCode {
    OutboxAlreadyClaimed,
    DispatchingAlreadyCommitted,
    WorkerStale,
    RouteStale,
    StoreCorrupt,
}

/// A journal read or write that could not be proven.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("dispatch journal: {0}")]
pub struct JournalError(pub String);

/// The per-worker retained journal the guard consumes (it never opens a
/// store of its own). Every mutation is a generation-fenced transaction in
/// a real implementation.
pub trait DispatchJournal {
    /// The closed dispatch facts for one binding.
    ///
    /// # Errors
    ///
    /// The facts cannot be read (the guard then treats the turn as
    /// possibly dispatched).
    fn read_dispatch_facts(&self, binding: &DispatchBinding)
        -> Result<DispatchFacts, JournalError>;

    /// Atomically assert no `dispatching`/`provider_entered` fact, claim the
    /// exact outbox, and commit one fresh `dispatching` fact: the one
    /// authorization for a physical provider call.
    ///
    /// # Errors
    ///
    /// The typed reason the claim could not be made exactly once.
    fn claim_and_commit_dispatching(&self, binding: &DispatchBinding) -> Result<(), ClaimCode>;

    /// Commit `provider_entered` evidence.
    ///
    /// # Errors
    ///
    /// The evidence could not be committed (no physical call follows).
    fn commit_provider_entered(&self, binding: &DispatchBinding) -> Result<(), JournalError>;
}

/// Why the guard refused before any claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotReady {
    NotArmed,
    OutboxUnavailable,
}

/// The guard's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome<T, E> {
    /// The one physical call was made and returned.
    Dispatched(T),
    /// The one physical call was made and failed: one provider effect.
    ProviderError(E),
    /// A `dispatching`/`provider_entered` fact exists (or the facts could not
    /// be read): no call; settle `execution_unknown` unless exact terminal
    /// evidence exists.
    AlreadyDispatched,
    /// Not armed, or no claimable outbox: no call.
    NotReady(NotReady),
    /// The atomic claim was lost to a fence or a race: no call.
    ClaimLost(ClaimCode),
    /// `dispatching` committed, then the writer fence was lost at the
    /// provider-effect boundary: terminal `execution_unknown`, no call, and
    /// the committed fact blocks any relaunch.
    FenceLostAfterDispatch,
    /// `dispatching` committed but the provider-entry evidence did not: no
    /// call (TS threw here), terminal `execution_unknown`.
    ProviderEntryUnrecorded,
}

/// Authorizes and performs the single physical provider dispatch of an
/// admitted turn.
pub struct RetainedDispatchGuard<J> {
    journal: J,
    assert_fence: Box<dyn Fn() -> bool + Send + Sync>,
}

impl<J: DispatchJournal> RetainedDispatchGuard<J> {
    /// A guard whose writer fence always holds.
    pub fn new(journal: J) -> RetainedDispatchGuard<J> {
        RetainedDispatchGuard::with_fence(journal, || true)
    }

    /// A guard re-checking `fence_holds` at the provider-effect boundary (a
    /// preflight check never authorizes a later call).
    pub fn with_fence(
        journal: J,
        fence_holds: impl Fn() -> bool + Send + Sync + 'static,
    ) -> RetainedDispatchGuard<J> {
        RetainedDispatchGuard {
            journal,
            assert_fence: Box::new(fence_holds),
        }
    }

    /// The journal.
    pub fn journal(&self) -> &J {
        &self.journal
    }

    /// Authorize and perform the one physical call for `binding`, or refuse
    /// without a call. `physical_call` runs at most once per admitted turn
    /// across all attempts and restarts.
    pub async fn dispatch_once<T, E, F, Fut>(
        &self,
        binding: &DispatchBinding,
        physical_call: F,
    ) -> DispatchOutcome<T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let Ok(facts) = self.journal.read_dispatch_facts(binding) else {
            return DispatchOutcome::AlreadyDispatched;
        };
        if facts.has_dispatching || facts.has_provider_entered {
            return DispatchOutcome::AlreadyDispatched;
        }
        if !facts.capture_armed {
            return DispatchOutcome::NotReady(NotReady::NotArmed);
        }
        if !facts.outbox_unclaimed {
            return DispatchOutcome::NotReady(NotReady::OutboxUnavailable);
        }
        if let Err(code) = self.journal.claim_and_commit_dispatching(binding) {
            return DispatchOutcome::ClaimLost(code);
        }
        if !(self.assert_fence)() {
            return DispatchOutcome::FenceLostAfterDispatch;
        }
        if self.journal.commit_provider_entered(binding).is_err() {
            return DispatchOutcome::ProviderEntryUnrecorded;
        }
        match physical_call().await {
            Ok(value) => DispatchOutcome::Dispatched(value),
            Err(error) => DispatchOutcome::ProviderError(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    /// An in-memory journal with the real journal's atomicity (one lock per
    /// mutation).
    #[derive(Default)]
    struct MemoryJournal {
        facts: Mutex<DispatchFacts>,
        steal_claim: AtomicBool,
        provider_entered: AtomicU32,
    }

    impl MemoryJournal {
        fn armed() -> MemoryJournal {
            MemoryJournal {
                facts: Mutex::new(DispatchFacts {
                    capture_armed: true,
                    outbox_unclaimed: true,
                    ..DispatchFacts::default()
                }),
                ..MemoryJournal::default()
            }
        }
    }

    impl DispatchJournal for Arc<MemoryJournal> {
        fn read_dispatch_facts(&self, _: &DispatchBinding) -> Result<DispatchFacts, JournalError> {
            Ok(*self.facts.lock().unwrap())
        }

        fn claim_and_commit_dispatching(&self, _: &DispatchBinding) -> Result<(), ClaimCode> {
            if self.steal_claim.load(Ordering::SeqCst) {
                return Err(ClaimCode::OutboxAlreadyClaimed);
            }
            let mut facts = self.facts.lock().unwrap();
            if facts.has_dispatching || facts.has_provider_entered {
                return Err(ClaimCode::DispatchingAlreadyCommitted);
            }
            if !facts.outbox_unclaimed {
                return Err(ClaimCode::OutboxAlreadyClaimed);
            }
            facts.outbox_unclaimed = false;
            facts.has_dispatching = true;
            Ok(())
        }

        fn commit_provider_entered(&self, _: &DispatchBinding) -> Result<(), JournalError> {
            self.facts.lock().unwrap().has_provider_entered = true;
            self.provider_entered.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn binding() -> DispatchBinding {
        DispatchBinding {
            rlm_child_id: "child1".to_string(),
            turn_id: "turn1".to_string(),
            admission_receipt_digest: format!("sha256:{}", "a".repeat(64)),
        }
    }

    async fn call(
        guard: &RetainedDispatchGuard<Arc<MemoryJournal>>,
        calls: &AtomicU32,
    ) -> DispatchOutcome<&'static str, String> {
        guard
            .dispatch_once(&binding(), || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok("ok")
            })
            .await
    }

    #[tokio::test]
    async fn the_physical_call_runs_exactly_once() {
        let journal = Arc::new(MemoryJournal::armed());
        let guard = RetainedDispatchGuard::new(Arc::clone(&journal));
        let calls = AtomicU32::new(0);
        assert_eq!(
            call(&guard, &calls).await,
            DispatchOutcome::Dispatched("ok")
        );
        // A committed dispatching fact blocks every later attempt, including
        // a fresh guard after a restart.
        assert_eq!(
            call(&guard, &calls).await,
            DispatchOutcome::AlreadyDispatched
        );
        let restarted = RetainedDispatchGuard::new(Arc::clone(&journal));
        for _ in 0..5 {
            assert_eq!(
                call(&restarted, &calls).await,
                DispatchOutcome::AlreadyDispatched
            );
        }
        assert_eq!(
            (
                calls.load(Ordering::SeqCst),
                journal.provider_entered.load(Ordering::SeqCst)
            ),
            (1, 1)
        );
    }

    #[tokio::test]
    async fn a_crash_after_dispatching_never_relaunches() {
        // dispatching committed, then the process died before the call.
        let journal = Arc::new(MemoryJournal::armed());
        journal.facts.lock().unwrap().has_dispatching = true;
        let guard = RetainedDispatchGuard::new(Arc::clone(&journal));
        let calls = AtomicU32::new(0);
        for _ in 0..3 {
            assert_eq!(
                call(&guard, &calls).await,
                DispatchOutcome::AlreadyDispatched
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn concurrent_attempts_make_one_physical_call() {
        let journal = Arc::new(MemoryJournal::armed());
        let guard = RetainedDispatchGuard::new(Arc::clone(&journal));
        let calls = AtomicU32::new(0);
        let outcomes = futures::future::join_all((0..8).map(|_| call(&guard, &calls))).await;
        let dispatched = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, DispatchOutcome::Dispatched(_)))
            .count();
        assert_eq!((dispatched, calls.load(Ordering::SeqCst)), (1, 1));
    }

    #[tokio::test]
    async fn a_provider_error_is_one_dispatch_and_never_relaunches() {
        let journal = Arc::new(MemoryJournal::armed());
        let guard = RetainedDispatchGuard::new(Arc::clone(&journal));
        let failed: DispatchOutcome<(), &str> = guard
            .dispatch_once(&binding(), || async { Err("boom") })
            .await;
        assert_eq!(failed, DispatchOutcome::ProviderError("boom"));
        let calls = AtomicU32::new(0);
        assert_eq!(
            call(&guard, &calls).await,
            DispatchOutcome::AlreadyDispatched
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_unarmed_turn_or_a_lost_claim_makes_no_call() {
        let calls = AtomicU32::new(0);
        let unarmed = RetainedDispatchGuard::new(Arc::new(MemoryJournal::default()));
        assert_eq!(
            call(&unarmed, &calls).await,
            DispatchOutcome::NotReady(NotReady::NotArmed)
        );
        let journal = Arc::new(MemoryJournal::armed());
        journal.steal_claim.store(true, Ordering::SeqCst);
        let raced = RetainedDispatchGuard::new(Arc::clone(&journal));
        assert_eq!(
            call(&raced, &calls).await,
            DispatchOutcome::ClaimLost(ClaimCode::OutboxAlreadyClaimed)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_fence_lost_after_the_claim_is_terminal_with_no_call() {
        let journal = Arc::new(MemoryJournal::armed());
        let guard = RetainedDispatchGuard::with_fence(Arc::clone(&journal), || false);
        let calls = AtomicU32::new(0);
        assert_eq!(
            call(&guard, &calls).await,
            DispatchOutcome::FenceLostAfterDispatch
        );
        // The committed dispatching fact blocks a relaunch under a good fence.
        let healthy = RetainedDispatchGuard::new(Arc::clone(&journal));
        assert_eq!(
            call(&healthy, &calls).await,
            DispatchOutcome::AlreadyDispatched
        );
        assert_eq!(
            (
                calls.load(Ordering::SeqCst),
                journal.provider_entered.load(Ordering::SeqCst)
            ),
            (0, 0)
        );
    }
}
