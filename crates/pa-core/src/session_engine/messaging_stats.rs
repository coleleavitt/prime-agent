//! Per-session messaging instrumentation (upstream #2352, TS
//! `messaging-stats.ts`): the counters behind `rlm.messaging_stats()`.
//!
//! The counters feed the swarm starvation eval's defense lines
//! ([`crate::swarm_eval`]) and the digest lane's controller: context share
//! (estimated agent-message tokens over working-context tokens), turn
//! share (agent-triggered model steps over all steps), and cost share
//! (agent-triggered step tokens over all step tokens). Instrumentation
//! only: no delivery behaviour depends on the snapshot beyond what a
//! consumer decides from it.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// TS `MESSAGING_STATS_WINDOW_MS`: the rolling `last5m` window.
pub const MESSAGING_STATS_WINDOW_MS: u64 = 5 * 60 * 1000;
/// The window rings' bucket width: events landing inside the same second
/// share one counted row, so a ring holds at most one row per second of
/// window however hot the path runs. (TS kept one timestamp per event in a
/// ring capped at 5,000 events, so its counts saturated there; the totals
/// here are plain counters and the windows stay exact at second
/// granularity.)
pub const MESSAGING_STATS_BUCKET_MS: u64 = 1_000;

/// The `rlm.messaging_stats()` snapshot (TS `MessagingStatsSnapshot`): the
/// session's arrival, step, and context totals plus the outbound send
/// counts. The swarm eval's defense lines score it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MessagingStatsSnapshot {
    pub arrivals: ArrivalCounts,
    pub model_steps: StepCounts,
    pub ingestion_steps: StepCounts,
    pub context: ContextShape,
    pub sends: SendCounts,
}

/// Accepted inbound agent messages (delivered or queued).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrivalCounts {
    pub total: u64,
    pub last5m: u64,
}

/// Completed model steps (and their usage tokens).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepCounts {
    pub total: u64,
    pub last5m: u64,
    pub tokens: u64,
}

/// The agent-message share of the working context. `context_tokens` (and
/// therefore `share`) is unknown until an assistant usage is recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextShape {
    pub estimated_agent_message_tokens: u64,
    pub context_tokens: Option<u64>,
    pub share: Option<f64>,
}

/// Outbound `agent_message.send` attempts and failures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendCounts {
    pub attempts: u64,
    pub failures: u64,
}

/// The working-context inputs of one snapshot: the newest assistant
/// usage's context tokens (`None` before any) and the chars/4 estimate of
/// the agent-message rows in the working context.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessagingContext {
    pub context_tokens: Option<u64>,
    pub estimated_agent_message_tokens: u64,
}

/// One counted slice of a window ring.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    start_ms: u64,
    count: u64,
}

/// A trailing-window event count at bucket granularity.
#[derive(Debug, Default)]
struct WindowRing {
    buckets: VecDeque<Bucket>,
}

impl WindowRing {
    /// A bucket leaves the window once even its newest possible event is
    /// older than the trailing window.
    fn expired(bucket: &Bucket, now_ms: u64) -> bool {
        now_ms.saturating_sub(bucket.start_ms + MESSAGING_STATS_BUCKET_MS)
            >= MESSAGING_STATS_WINDOW_MS
    }

    fn record(&mut self, now_ms: u64) {
        while self
            .buckets
            .front()
            .is_some_and(|bucket| Self::expired(bucket, now_ms))
        {
            self.buckets.pop_front();
        }
        let start_ms = now_ms / MESSAGING_STATS_BUCKET_MS * MESSAGING_STATS_BUCKET_MS;
        match self.buckets.back_mut() {
            Some(bucket) if bucket.start_ms == start_ms => bucket.count += 1,
            _ => self.buckets.push_back(Bucket { start_ms, count: 1 }),
        }
    }

    fn count(&self, now_ms: u64) -> u64 {
        self.buckets
            .iter()
            .filter(|bucket| !Self::expired(bucket, now_ms))
            .map(|bucket| bucket.count)
            .sum()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.len()
    }
}

/// One step family's totals and window.
#[derive(Debug, Default)]
struct StepCounter {
    total: u64,
    tokens: u64,
    window: WindowRing,
}

impl StepCounter {
    fn record(&mut self, tokens: u64, now_ms: u64) {
        self.total += 1;
        self.tokens = self.tokens.saturating_add(tokens);
        self.window.record(now_ms);
    }

    fn counts(&self, now_ms: u64) -> StepCounts {
        StepCounts {
            total: self.total,
            last5m: self.window.count(now_ms),
            tokens: self.tokens,
        }
    }
}

#[derive(Debug, Default)]
struct Counters {
    arrivals_total: u64,
    arrivals: WindowRing,
    steps: StepCounter,
    ingestion_steps: StepCounter,
    send_attempts: u64,
    send_failures: u64,
}

/// The session's messaging counters (TS `MessagingStats`): totals, bounded
/// window rings, and the send totals behind one leaf mutex — never held
/// while any other lock is taken, so a caller may count under its own
/// locks.
#[derive(Debug, Default)]
pub struct MessagingStats {
    counters: std::sync::Mutex<Counters>,
}

impl MessagingStats {
    fn counters(&self) -> std::sync::MutexGuard<'_, Counters> {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// One accepted inbound agent message (delivered or queued; never a
    /// rejected delivery).
    pub fn record_arrival(&self, now_ms: u64) {
        let mut counters = self.counters();
        counters.arrivals_total += 1;
        counters.arrivals.record(now_ms);
    }

    /// One completed model step (an assistant reply that did not end in
    /// `error`) with its usage tokens; `ingestion` when the step's run was
    /// triggered by an agent message.
    pub fn record_model_step(&self, tokens: u64, ingestion: bool, now_ms: u64) {
        let mut counters = self.counters();
        counters.steps.record(tokens, now_ms);
        if ingestion {
            counters.ingestion_steps.record(tokens, now_ms);
        }
    }

    /// One outbound `agent_message.send` attempt, counted at resolution; a
    /// failed one counts both.
    pub fn record_send_attempt(&self, failed: bool) {
        let mut counters = self.counters();
        counters.send_attempts += 1;
        counters.send_failures += u64::from(failed);
    }

    /// Forget everything (a session replacement: the retired session's
    /// counters never leak into the replacement's).
    pub fn reset(&self) {
        *self.counters() = Counters::default();
    }

    /// The snapshot at `now_ms` over the caller's working-context inputs.
    #[must_use]
    pub fn snapshot(&self, context: MessagingContext, now_ms: u64) -> MessagingStatsSnapshot {
        let counters = self.counters();
        MessagingStatsSnapshot {
            arrivals: ArrivalCounts {
                total: counters.arrivals_total,
                last5m: counters.arrivals.count(now_ms),
            },
            model_steps: counters.steps.counts(now_ms),
            ingestion_steps: counters.ingestion_steps.counts(now_ms),
            context: ContextShape {
                estimated_agent_message_tokens: context.estimated_agent_message_tokens,
                context_tokens: context.context_tokens,
                share: context
                    .context_tokens
                    .filter(|tokens| *tokens > 0)
                    .map(|tokens| {
                        (context.estimated_agent_message_tokens as f64 / tokens as f64).min(1.0)
                    }),
            },
            sends: SendCounts {
                attempts: counters.send_attempts,
                failures: counters.send_failures,
            },
        }
    }

    #[cfg(test)]
    fn arrival_buckets(&self) -> usize {
        self.counters().arrivals.len()
    }
}

/// TS `estimateMessagingTokens`: ~4 characters per token, rounded up. The
/// characters are UTF-16 code units (TS `string.length`).
#[must_use]
pub fn estimate_messaging_tokens(utf16_units: u64) -> u64 {
    utf16_units.div_ceil(4)
}

#[cfg(test)]
mod tests;
