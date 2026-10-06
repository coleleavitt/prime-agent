use super::*;

const T0: u64 = 1_000_000_000;

/// TS "counts arrivals, steps, and windows": totals cover every event,
/// `last5m` only those inside the trailing window, and the context share is
/// the estimate over the context tokens.
#[test]
fn counts_arrivals_steps_and_windows() {
    let stats = MessagingStats::default();
    let late = T0 + MESSAGING_STATS_WINDOW_MS + 6_000;
    stats.record_arrival(T0);
    stats.record_arrival(T0 + 500);
    stats.record_arrival(late);
    stats.record_model_step(800, true, T0);
    stats.record_model_step(600, false, T0 + 600);
    stats.record_model_step(400, false, late);
    let snapshot = stats.snapshot(
        MessagingContext {
            context_tokens: Some(2_000),
            estimated_agent_message_tokens: 200,
        },
        late + 500,
    );
    assert_eq!(
        snapshot,
        MessagingStatsSnapshot {
            arrivals: ArrivalCounts {
                total: 3,
                last5m: 1
            },
            model_steps: StepCounts {
                total: 3,
                last5m: 1,
                tokens: 1_800
            },
            ingestion_steps: StepCounts {
                total: 1,
                last5m: 0,
                tokens: 800
            },
            context: ContextShape {
                estimated_agent_message_tokens: 200,
                context_tokens: Some(2_000),
                share: Some(0.1),
            },
            sends: SendCounts::default(),
        }
    );
    assert_eq!(estimate_messaging_tokens(801), 201);
}

/// TS "reports a null share without context tokens and counts send
/// outcomes"; the share also caps at 1 (TS `Math.min(1, ...)`).
#[test]
fn a_null_share_without_context_tokens_and_send_outcomes() {
    let stats = MessagingStats::default();
    stats.record_send_attempt(false);
    stats.record_send_attempt(true);
    let unknown = stats.snapshot(
        MessagingContext {
            context_tokens: None,
            estimated_agent_message_tokens: 50,
        },
        T0,
    );
    let saturated = stats.snapshot(
        MessagingContext {
            context_tokens: Some(10),
            estimated_agent_message_tokens: 50,
        },
        T0,
    );
    assert_eq!(
        (unknown.context, unknown.sends, saturated.context.share),
        (
            ContextShape {
                estimated_agent_message_tokens: 50,
                context_tokens: None,
                share: None,
            },
            SendCounts {
                attempts: 2,
                failures: 1
            },
            Some(1.0),
        )
    );
}

/// The window rings stay bounded under sustained traffic (one counted row
/// per second of window) while the totals and the trailing count stay
/// exact; aged-out buckets leave the window.
#[test]
fn the_window_rings_stay_bounded_and_exact_under_sustained_traffic() {
    let stats = MessagingStats::default();
    let now = 100 * MESSAGING_STATS_WINDOW_MS;
    // 30_000 arrivals and steps spread across the window (100 per second).
    for index in 0..30_000u64 {
        let at = now - MESSAGING_STATS_WINDOW_MS + index / 100;
        stats.record_arrival(at);
        stats.record_model_step(2, true, at);
    }
    let snapshot = stats.snapshot(MessagingContext::default(), now);
    let later = stats.snapshot(MessagingContext::default(), now + MESSAGING_STATS_WINDOW_MS);
    assert_eq!(
        (
            snapshot.arrivals,
            snapshot.ingestion_steps,
            later.arrivals.last5m,
            later.model_steps.last5m,
        ),
        (
            ArrivalCounts {
                total: 30_000,
                last5m: 30_000
            },
            StepCounts {
                total: 30_000,
                last5m: 30_000,
                tokens: 60_000
            },
            0,
            0,
        )
    );
    assert!(
        stats.arrival_buckets()
            <= (MESSAGING_STATS_WINDOW_MS / MESSAGING_STATS_BUCKET_MS) as usize + 1,
        "the ring grew past its per-bucket bound: {}",
        stats.arrival_buckets()
    );
}

/// The wire shape is the TS JSON exactly (`context_tokens`/`share` are
/// `null` until known), the shape `rlm.messaging_stats()` returns.
#[test]
fn the_snapshot_serializes_to_the_ts_wire_shape() {
    let snapshot = MessagingStats::default().snapshot(MessagingContext::default(), T0);
    assert_eq!(
        serde_json::to_value(snapshot).unwrap(),
        serde_json::json!({
            "arrivals": { "total": 0, "last5m": 0 },
            "model_steps": { "total": 0, "last5m": 0, "tokens": 0 },
            "ingestion_steps": { "total": 0, "last5m": 0, "tokens": 0 },
            "context": { "estimated_agent_message_tokens": 0, "context_tokens": null, "share": null },
            "sends": { "attempts": 0, "failures": 0 },
        })
    );
}

/// A reset forgets every counter (a session replacement).
#[test]
fn a_reset_forgets_every_counter() {
    let stats = MessagingStats::default();
    stats.record_arrival(T0);
    stats.record_model_step(5, true, T0);
    stats.record_send_attempt(true);
    stats.reset();
    assert_eq!(
        stats.snapshot(MessagingContext::default(), T0),
        MessagingStats::default().snapshot(MessagingContext::default(), T0)
    );
}
