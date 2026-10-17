//! Idle compaction: compact a parked session in the background before its
//! prompt cache lapses, so the next prompt pays for a short context
//! instead of re-reading a long one uncached.
//!
//! Claude Code 2.1.296 (`kasbfbhj:dDt`/`Mao`/`pDt`, gate
//! `tengu_sunny_locket` {mode off|log|compact, minTokens, fireAtFraction},
//! `CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS`, remote `CLAUDE_CODE_SLEEP_COMPACT`)
//! arms a timer after each request at `fireAtFraction` (0.9) of the
//! one-hour cache TTL and, when it fires on a still-idle session holding at
//! least `minTokens` (200k by default, floor 100k), compacts it; a newer
//! request, a late fire, or a near-limit account cancels it.
//!
//! Here the worker's parked turn runner arms the same timer beside the idle
//! passivation's: `experimental.idleCompaction` (or
//! `PRIME_AGENT_IDLE_COMPACT`) turns it on, `idleCompactionMinutes` (54, 0.9
//! of an hour) or `idleCompactionAfterMs` sets the idle period, and
//! `idleCompactionMinTokens` (or `PRIME_AGENT_IDLE_COMPACT_MIN_TOKENS`) the
//! floor. It runs at most once per idle period, never while a turn runs,
//! input waits, a compaction runs, a background `bash()` lives, or the
//! worker shuts down; it compacts through the `/compact` path and reports
//! as an automatic (`threshold`) compaction.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pa_types::sync::MutexExt as _;

/// The turn runner's idle-compaction state.
pub(crate) struct IdleCompaction {
    manager: Arc<crate::compaction::CompactionManager>,
    /// The `last_activity_ms` the last idle compaction ran for: one run per
    /// idle period (`0` = none yet).
    compacted_for_activity: AtomicU64,
}

impl IdleCompaction {
    pub(crate) fn new(manager: Arc<crate::compaction::CompactionManager>) -> Self {
        Self {
            manager,
            compacted_for_activity: AtomicU64::new(0),
        }
    }
}

/// An idle compaction over a manager of its own (the turn-runner test
/// fixtures).
#[cfg(test)]
pub(crate) fn test_idle_compaction(
    engine: &Arc<dyn crate::engine::SessionEngine>,
) -> IdleCompaction {
    IdleCompaction::new(Arc::new(crate::compaction::CompactionManager::new(
        Arc::clone(engine),
        Arc::new(super::EventPump::new()),
        Arc::new(std::sync::Mutex::new(super::SessionCore::test_core(
            None,
            String::new(),
        ))),
        String::new(),
        std::path::PathBuf::from("/tmp"),
    )))
}

/// Sleep for `duration`, or forever when there is none (a disarmed timer in
/// a `select!`).
pub(super) async fn sleep_for(duration: Option<std::time::Duration>) {
    match duration {
        Some(duration) => tokio::time::sleep(duration).await,
        None => std::future::pending().await,
    }
}

/// The parked session's state the idle compaction gates on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdleState {
    pub(crate) busy: bool,
    pub(crate) last_activity_ms: u64,
    pub(crate) compacted_for_activity: u64,
    pub(crate) now_ms: u64,
}

/// How long until the idle compaction is due: `None` when it must not arm
/// (off, busy, or already run for this idle period).
pub(crate) fn idle_compaction_delay(
    enabled: bool,
    after_ms: u64,
    state: IdleState,
) -> Option<std::time::Duration> {
    if !enabled || state.busy || state.compacted_for_activity == state.last_activity_ms {
        return None;
    }
    let idle_ms = state.now_ms.saturating_sub(state.last_activity_ms);
    Some(std::time::Duration::from_millis(
        after_ms.saturating_sub(idle_ms),
    ))
}

/// Whether the context is large enough to compact: an unknown size (no
/// window, or right after a compaction) never is.
pub(crate) fn context_large_enough(tokens: Option<u64>, min_tokens: u64) -> bool {
    tokens.is_some_and(|tokens| tokens >= min_tokens)
}

impl super::turn::TurnRunner {
    /// Busy, for the idle compaction: compacting, shutting down, queued
    /// input or a paused pump, or a live background `bash()`.
    fn idle_busy(&self) -> (bool, u64, std::path::PathBuf) {
        let core = self.core.lock_or_recover();
        let busy = core.compacting
            || core.shutdown_requested
            || !core.steering.is_empty()
            || !core.follow_up.is_empty()
            || !core.pending_next_turn.is_empty()
            || core.queued_input_suspended
            || self.input_pauses.paused()
            || self.user_bash.is_running();
        (
            busy,
            core.last_activity_ms,
            std::path::PathBuf::from(&core.cwd),
        )
    }

    /// The idle-compaction timer for the parked session; `None` disarms it.
    pub(super) fn idle_compaction_window(&self) -> Option<std::time::Duration> {
        let (busy, last_activity_ms, cwd) = self.idle_busy();
        if busy {
            return None;
        }
        let experimental =
            pa_core::settings::SettingsManager::create(&cwd, &self.passivation.agent_dir)
                .experimental();
        idle_compaction_delay(
            experimental.idle_compaction,
            experimental.idle_compaction_after_ms,
            IdleState {
                busy,
                last_activity_ms,
                compacted_for_activity: self
                    .idle_compaction
                    .compacted_for_activity
                    .load(Ordering::SeqCst),
                now_ms: crate::util::now_ms(),
            },
        )
    }

    /// The timer fired: re-check every gate on a fresh snapshot, then
    /// compact when the context holds at least the floor.
    pub(super) async fn maybe_run_idle_compaction(&self) {
        let (busy, last_activity_ms, cwd) = self.idle_busy();
        let experimental =
            pa_core::settings::SettingsManager::create(&cwd, &self.passivation.agent_dir)
                .experimental();
        let due = idle_compaction_delay(
            experimental.idle_compaction,
            experimental.idle_compaction_after_ms,
            IdleState {
                busy,
                last_activity_ms,
                compacted_for_activity: self
                    .idle_compaction
                    .compacted_for_activity
                    .load(Ordering::SeqCst),
                now_ms: crate::util::now_ms(),
            },
        );
        if due != Some(std::time::Duration::ZERO) {
            return;
        }
        // One attempt per idle period, whatever it decides.
        self.idle_compaction
            .compacted_for_activity
            .store(last_activity_ms, Ordering::SeqCst);
        let tokens = {
            let core = self.core.lock_or_recover();
            core.store.as_ref().and_then(|store| {
                crate::session_stats::store_context_usage(store, self.engine.model_context_window())
                    .and_then(|usage| usage.get("tokens").and_then(serde_json::Value::as_u64))
            })
        };
        if !context_large_enough(tokens, experimental.idle_compaction_min_tokens) {
            return;
        }
        tracing::info!(
            target: "pa_daemon::idle_compaction",
            tokens = tokens.unwrap_or_default(),
            "compacting the idle session before its prompt cache lapses"
        );
        let outcome = self
            .idle_compaction
            .manager
            .run_with_reason("threshold", None, &self.idle_notify)
            .await;
        // The run's end restamped nothing: keep this idle period spent so
        // the next park does not compact again.
        let restamped = self.core.lock_or_recover().last_activity_ms;
        self.idle_compaction
            .compacted_for_activity
            .store(restamped, Ordering::SeqCst);
        let outcome = match &outcome {
            crate::engine::CompactionOutcome::Compacted { .. } => "compacted",
            crate::engine::CompactionOutcome::Skipped { .. } => "skipped",
            crate::engine::CompactionOutcome::Failed { .. } => "failed",
            crate::engine::CompactionOutcome::Aborted => "aborted",
        };
        tracing::info!(target: "pa_daemon::idle_compaction", outcome, "idle compaction settled");
        self.work_notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(busy: bool, last: u64, compacted: u64, now: u64) -> IdleState {
        IdleState {
            busy,
            last_activity_ms: last,
            compacted_for_activity: compacted,
            now_ms: now,
        }
    }

    #[test]
    fn the_timer_arms_for_the_rest_of_the_idle_period() {
        assert_eq!(
            idle_compaction_delay(true, 60_000, state(false, 1_000, 0, 21_000)),
            Some(std::time::Duration::from_secs(40))
        );
        assert_eq!(
            idle_compaction_delay(true, 60_000, state(false, 1_000, 0, 90_000)),
            Some(std::time::Duration::ZERO),
            "an overdue period fires at once"
        );
    }

    #[test]
    fn off_busy_or_already_compacted_never_arms() {
        assert_eq!(
            idle_compaction_delay(false, 60_000, state(false, 1_000, 0, 2_000)),
            None
        );
        assert_eq!(
            idle_compaction_delay(true, 60_000, state(true, 1_000, 0, 2_000)),
            None
        );
        assert_eq!(
            idle_compaction_delay(true, 60_000, state(false, 1_000, 1_000, 2_000)),
            None,
            "one run per idle period"
        );
        assert_eq!(
            idle_compaction_delay(true, 60_000, state(false, 5_000, 1_000, 6_000)),
            Some(std::time::Duration::from_secs(59)),
            "new activity opens a new period"
        );
    }

    #[test]
    fn only_a_known_context_at_the_floor_compacts() {
        assert!(context_large_enough(Some(200_000), 200_000));
        assert!(!context_large_enough(Some(199_999), 200_000));
        assert!(!context_large_enough(None, 100_000));
    }

    #[tokio::test]
    async fn a_disarmed_timer_never_fires() {
        let fired = tokio::time::timeout(std::time::Duration::from_millis(20), sleep_for(None))
            .await
            .is_ok();
        assert!(!fired);
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            sleep_for(Some(std::time::Duration::from_millis(1))),
        )
        .await
        .unwrap();
    }
}
