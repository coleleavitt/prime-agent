//! The auto-retry unit battery: the retry-loop event surface, the
//! park seam, and the stream-drop retry pins.
use std::sync::{Arc, Mutex};

use pa_agent::types::{AssistantContent, AssistantMessageDiagnostic, TextContent, Usage};

use super::super::provider_retry::UNBOUNDED_BACKOFF_MS;
use super::*;

fn error_message(
    kind: Option<&str>,
    status: Option<u16>,
    retry_after_ms: Option<u64>,
) -> AssistantMessage {
    let details = serde_json::json!({
        "kind": kind,
        "status": status,
        "retryAfterMs": retry_after_ms,
    });
    AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        })],
        api: String::new(),
        provider: "test".to_string(),
        model: "m".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: Some(vec![AssistantMessageDiagnostic {
            kind: "provider_stream_failure".to_string(),
            timestamp: 0,
            error: None,
            details: Some(details),
        }]),
        usage: Usage::zero(),
        stop_reason: StopReason::Error,
        stop_reason_raw: None,
        error_message: Some("provider down".to_string()),
        timestamp: 0,
        discarded_usage: None,
    }
}

fn ok_message() -> AssistantMessage {
    AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: "done".to_string(),
            text_signature: None,
        })],
        api: String::new(),
        provider: "test".to_string(),
        model: "m".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::zero(),
        stop_reason: StopReason::Stop,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
        discarded_usage: None,
    }
}

fn fast_policy() -> ProviderRetryPolicy {
    ProviderRetryPolicy {
        enabled: true,
        max_retries: 3,
        base_delay_ms: 5,
        max_retry_delay_ms: 50,
        max_delay_ms: UNBOUNDED_BACKOFF_MS,
        connection_wait_ms: 0,
    }
}

/// A `rate_limit` failure whose server-requested wait exceeds the cap
/// parks the session when the park seam reports a park: the give-up
/// status becomes the parked sentence (TS #2375).
#[tokio::test]
async fn quota_reset_beyond_cap_parks_through_the_seam() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let parked_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events_for_emit = Arc::clone(&events);
    let parked_calls_for_seam = Arc::clone(&parked_calls);
    let mut seam = move |message: AssistantMessage, abort: &str| {
        let parked_calls = Arc::clone(&parked_calls_for_seam);
        let abort = abort.to_string();
        Box::pin(async move {
            parked_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // The seam sees the failed message and the give-up
            // sentence; the park answers with the parked status.
            assert_eq!(message.stop_reason, StopReason::Error);
            assert!(abort.contains("Provider requested a 4363s wait"));
            Some(crate::session_engine::provider_park::ProviderParkOutcome {
                status_message: format!(
                    "{abort}. Session parked until 2026-09-24T00:00:00.000Z and will resume automatically: {}",
                    message.error_message.as_deref().unwrap_or("unknown error"),
                ),
            })
        }) as crate::session_engine::provider_park::ParkFuture
    };
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || async { Ok(error_message(Some("rate_limit"), None, Some(4_363_000))) },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        Some(&mut seam),
    )
    .await
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        parked_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the park seam runs exactly once at the give-up"
    );
    let events = events.lock().unwrap().clone();
    // One parked end, no retry starts.
    assert_eq!(events.len(), 1, "one parked end: {events:?}");
    let final_error = match events.as_slice() {
        [
            AutoRetryEvent::End {
                success: false,
                final_error: Some(final_error),
                ..
            },
        ] => final_error.clone(),
        other => panic!("expected one parked end, got {other:?}"),
    };
    assert!(final_error.contains("Session parked until 2026-09-24T00:00:00.000Z"));
}

/// A park seam that declines keeps the give-up: the surfaced status
/// stays the plain exceeds-cap sentence.
#[tokio::test]
async fn quota_reset_beyond_cap_keeps_the_give_up_when_the_seam_declines() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let mut seam = move |_message: AssistantMessage, _abort: &str| {
        Box::pin(async move { None }) as crate::session_engine::provider_park::ParkFuture
    };
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || async { Ok(error_message(Some("rate_limit"), None, Some(4_363_000))) },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        Some(&mut seam),
    )
    .await
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::Error);
    let events = events.lock().unwrap().clone();
    let final_error = match events.as_slice() {
        [
            AutoRetryEvent::End {
                success: false,
                final_error: Some(final_error),
                ..
            },
        ] => final_error.clone(),
        other => panic!("expected one give-up end, got {other:?}"),
    };
    assert!(final_error.contains("Provider requested a 4363s wait before retrying"));
    assert!(!final_error.contains("Session parked"));
}

/// The park seam is only consulted for quota failures: a
/// server-requested wait on a non-quota failure keeps the give-up
/// even when a park is armed (TS parks only from the usage path).
#[tokio::test]
async fn non_quota_exceeds_cap_never_consults_the_park_seam() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let seam_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seam_ran_for_seam = Arc::clone(&seam_ran);
    let mut seam = move |_message: AssistantMessage, _abort: &str| {
        let seam_ran = Arc::clone(&seam_ran_for_seam);
        Box::pin(async move {
            seam_ran.store(true, std::sync::atomic::Ordering::SeqCst);
            None
        }) as crate::session_engine::provider_park::ParkFuture
    };
    let _ = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || async { Ok(error_message(Some("server_error"), None, Some(4_363_000))) },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        Some(&mut seam),
    )
    .await
    .unwrap();
    assert!(
        !seam_ran.load(std::sync::atomic::Ordering::SeqCst),
        "the park seam must not run for a non-quota failure"
    );
}

#[tokio::test]
async fn transient_failure_is_retried_until_success_with_events() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if attempt < 3 {
                    Ok(error_message(Some("server_error"), Some(500), None))
                } else {
                    Ok(ok_message())
                }
            }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(message.stop_reason, StopReason::Stop);
    // Two retry starts (one per retry) and one success end. The delays
    // sit in the jitter band around the 5ms/10ms ladder steps
    // (jittered: [4, 7] and [8, 14] with the ±20% rounding headroom).
    let events = events.lock().unwrap().clone();
    assert_eq!(events.len(), 3, "two starts + one end: {events:?}");
    let shape_matches = matches!(
        events.as_slice(),
        [
            AutoRetryEvent::Start {
                attempt: 1,
                max_attempts: 3,
                error_message: error_one,
                reason: RetryStartReason::Quick,
                ..
            },
            AutoRetryEvent::Start {
                attempt: 2,
                max_attempts: 3,
                error_message: error_two,
                reason: RetryStartReason::Quick,
                ..
            },
            AutoRetryEvent::End {
                success: true,
                attempt: 2,
                final_error: None,
                restored_model: None,
            },
        ] if error_one == "provider down" && error_two == "provider down"
    );
    assert!(shape_matches, "unexpected events: {events:?}");
    let delays: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            AutoRetryEvent::Start { delay_ms, .. } => Some(*delay_ms),
            AutoRetryEvent::End { .. } => None,
        })
        .collect();
    assert_eq!(delays.len(), 2, "two retry starts: {events:?}");
    assert!(
        (4..=7).contains(&delays[0]) && (8..=14).contains(&delays[1]),
        "jittered delays {delays:?} outside the [4,7]/[8,14] bands"
    );
}

#[tokio::test]
async fn exhausted_retries_surface_the_final_error() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(error_message(Some("server_error"), None, None))
            }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    // One initial attempt plus three retries.
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 4);
    assert_eq!(message.stop_reason, StopReason::Error);
    let events = events.lock().unwrap().clone();
    assert_eq!(
        events.last(),
        Some(&AutoRetryEvent::End {
            success: false,
            attempt: 3,
            final_error: Some("provider down".to_string()),
            restored_model: None,
        })
    );
    assert_eq!(events.len(), 4); // three starts + one end
}

/// A failed turn carrying the `stream_drop` class (the provider ended
/// the response stream mid-block without a stop signal).
fn stream_drop_message() -> AssistantMessage {
    let details = serde_json::json!({
        "kind": "stream_drop",
        "providerErrorType": "stream_drop",
    });
    AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        })],
        api: String::new(),
        provider: "test".to_string(),
        model: "m".to_string(),
        response_model: None,
        response_id: None,
        diagnostics: Some(vec![AssistantMessageDiagnostic {
            kind: "provider_stream_failure".to_string(),
            timestamp: 0,
            error: None,
            details: Some(details),
        }]),
        usage: Usage::zero(),
        stop_reason: StopReason::Error,
        stop_reason_raw: None,
        error_message: Some(
            "Provider dropped the response stream (stream_drop): the stream ended inside a thinking block before the stop signal"
                .to_string(),
        ),
        timestamp: 0,
        discarded_usage: None,
    }
}

/// The stream-drop pin (a): a provider that ends the stream mid-block
/// (the retryable `stream_drop` class) is retried — the re-issued turn
/// completes on the retry, never settling as the silent empty turn.
#[tokio::test]
async fn stream_drop_retries_until_the_turn_completes() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                let attempt = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if attempt < 2 {
                    Ok(stream_drop_message())
                } else {
                    Ok(ok_message())
                }
            }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(message.stop_reason, StopReason::Stop);
    let events = events.lock().unwrap().clone();
    // One retry start (naming the class), one success end.
    assert!(
        matches!(
            events.as_slice(),
            [
                AutoRetryEvent::Start {
                    attempt: 1,
                    error_message,
                    ..
                },
                AutoRetryEvent::End {
                    success: true,
                    attempt: 1,
                    final_error: None,
                    restored_model: None,
                }
            ] if error_message.contains("stream_drop")
                && error_message.contains("thinking block")
        ),
        "the retry fired for the drop: {events:?}"
    );
}

/// The stream-drop pin (b): when the drop exhausts the retries, the
/// failure row names the `stream_drop` class — never a silent empty
/// turn.
#[tokio::test]
async fn stream_drop_exhausts_retries_surfacing_the_class() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(stream_drop_message())
            }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    // One initial attempt plus three retries.
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 4);
    assert_eq!(message.stop_reason, StopReason::Error);
    let events = events.lock().unwrap().clone();
    let final_error = match events.last() {
        Some(AutoRetryEvent::End {
            success: false,
            attempt: 3,
            final_error: Some(final_error),
            restored_model: None,
        }) => final_error.clone(),
        other => panic!("the exhaustion must disclose the class: {other:?}"),
    };
    assert!(
        final_error.contains("stream_drop") && final_error.contains("thinking block"),
        "the failure row names the stream_drop class: {final_error}"
    );
    assert_eq!(events.len(), 4); // three starts + one end
}

/// SANCTIONED DIVERGENCE (the 402 diagnosis): a permanent provider
/// failure on the first attempt still emits the failure-scoped
/// `auto_retry_end` (attempt 0) — the disclosure row must fire for
/// every provider failure, not only retry episodes.
#[tokio::test]
async fn permanent_failures_never_retry_but_disclose() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(error_message(Some("invalid_request"), Some(400), None))
            }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(message.stop_reason, StopReason::Error);
    // No retry happened, but the failure still discloses at attempt 0:
    // the outcome row is failure-scoped, not episode-scoped.
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[AutoRetryEvent::End {
            success: false,
            attempt: 0,
            final_error: Some("provider down".to_string()),
            restored_model: None,
        }]
    );
}

/// THE 402 REGRESSION (the diagnosis's variant B): a wallet-drain 402
/// (the `payment_required` kind, classified by status regardless of
/// the body's `error.type` text) is permanent on the FIRST attempt —
/// no retry ladder burns 13-15s on a dead wallet — and the
/// failure-scoped disclosure still fires, so the turn never settles
/// as a silent empty message.
#[tokio::test]
async fn payment_failures_settle_once_with_the_disclosure() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(error_message(Some("payment_required"), Some(402), None))
            }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[AutoRetryEvent::End {
            success: false,
            attempt: 0,
            final_error: Some("provider down".to_string()),
            restored_model: None,
        }]
    );
}

/// TS #2472: a safety-filter failure (e.g. a `content_filter`
/// rejection) is a deterministic rejection — one attempt, no retry
/// loop — with the failure-scoped disclosure (attempt 0).
#[tokio::test]
async fn safety_failures_are_permanent_never_retry_but_disclose() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(error_message(Some("safety"), Some(400), None))
            }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a safety rejection must not retry"
    );
    assert_eq!(message.stop_reason, StopReason::Error);
    // The failure-scoped disclosure still fires at attempt 0.
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[AutoRetryEvent::End {
            success: false,
            attempt: 0,
            final_error: Some("provider down".to_string()),
            restored_model: None,
        }]
    );
}

/// A context overflow can never succeed unchanged (TS
/// `_isRetryableError`'s overflow guard): the turn surfaces the error
/// immediately so the compact-and-retry recovery owns it.
#[tokio::test]
async fn context_overflow_never_enters_the_retry_loop() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        200_000,
        None,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut overflow = error_message(None, None, None);
                overflow.diagnostics = None;
                overflow.error_message =
                    Some("prompt is too long: 213462 tokens > 200000 maximum".to_string());
                Ok(overflow)
            }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cancelled_wait_aborts_with_retry_cancelled() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let mut attempts = 0;
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || {
            attempts += 1;
            async { Ok(error_message(Some("server_error"), None, None)) }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { false },
        None,
    )
    .await
    .unwrap();
    assert_eq!(attempts, 1);
    assert_eq!(message.stop_reason, StopReason::Aborted);
    // TS `_retryAfterDelay`'s abort path closes the started retry.
    assert_eq!(
        events.lock().unwrap().last(),
        Some(&AutoRetryEvent::End {
            success: false,
            attempt: 1,
            final_error: Some("Retry cancelled".to_string()),
            restored_model: None,
        })
    );
}

#[tokio::test]
async fn aborted_signal_racing_failure_stops_aborted() {
    let controller = pa_agent::abort::AbortController::new();
    controller.abort();
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        Some(&controller.signal()),
        || async { Ok(error_message(Some("server_error"), None, None)) },
        |_| async { Ok(()) },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    assert_eq!(message.stop_reason, StopReason::Aborted);
}

#[tokio::test]
async fn server_retry_after_over_cap_ends_the_loop() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(error_message(Some("rate_limit"), Some(429), Some(500)))
            }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    // The cap (50ms) rejects the server's 500ms wait on the first retry
    // request; the loop stops and reports the refused wait (TS
    // `exceeds-cap` end event, attempt 0 because no retry finished).
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[AutoRetryEvent::End {
            success: false,
            attempt: 0,
            final_error: Some(
                "Provider requested a 1s wait before retrying (above retry.provider.maxRetryDelayMs=50ms): provider down"
                    .to_string(),
            ),
            restored_model: None,
        }]
    );
}

/// A disabled retry policy never retries, but a provider failure
/// still settles with the failure-scoped disclosure (attempt 0).
#[tokio::test]
async fn disabled_policy_never_retries_but_discloses() {
    let policy = ProviderRetryPolicy {
        enabled: false,
        ..fast_policy()
    };
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let message = run_turn_with_auto_retry(
        &policy,
        0,
        None,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(error_message(Some("server_error"), None, None))
            }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[AutoRetryEvent::End {
            success: false,
            attempt: 0,
            final_error: Some("provider down".to_string()),
            restored_model: None,
        }]
    );
}

#[tokio::test]
async fn attempt_errors_propagate() {
    let error = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || async { Err(anyhow::anyhow!("turn crashed")) },
        |_| async { Ok(()) },
        |_| async { true },
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(error.to_string(), "turn crashed");
}

/// The router's tool-use 404 ("No endpoints found that support tool
/// use") is a permanent capability mismatch: the turn surfaces after
/// one attempt with the failure-scoped disclosure (attempt 0), like
/// the other non-retryable provider failures.
#[tokio::test]
async fn unsupported_tool_failures_surface_with_the_disclosure() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_for_emit = Arc::clone(&events);
    let message = run_turn_with_auto_retry(
        &fast_policy(),
        0,
        None,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut unsupported = error_message(Some("invalid_request"), Some(404), None);
                unsupported.error_message =
                    Some("404 No endpoints found that support tool use.".to_string());
                Ok(unsupported)
            }
        },
        move |event| {
            let events = Arc::clone(&events_for_emit);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        |_| async { true },
        None,
    )
    .await
    .unwrap();
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(message.stop_reason, StopReason::Error);
    // A provider rejection with a recorded stream failure discloses
    // even though no retry ran.
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[AutoRetryEvent::End {
            success: false,
            attempt: 0,
            final_error: Some("404 No endpoints found that support tool use.".to_string()),
            restored_model: None,
        }]
    );
}

/// A failed turn whose request produced no response (the provider's HTTP
/// layer records the `transport` facts): a refused, reset or timed-out
/// connection.
fn connection_failure_message() -> AssistantMessage {
    let mut message = error_message(Some("unknown"), None, None);
    message.error_message = Some("Connection error.".to_string());
    if let Some(details) = message
        .diagnostics
        .as_mut()
        .and_then(|diagnostics| diagnostics[0].details.as_mut())
    {
        details["transport"] = serde_json::json!({
            "class": "connect",
            "cause": "tcp connect error: Network is unreachable (os error 101)",
        });
    }
    message
}

fn outage_policy(connection_wait_ms: u64) -> ProviderRetryPolicy {
    ProviderRetryPolicy {
        connection_wait_ms,
        ..fast_policy()
    }
}

/// Drive the loop over `outcomes` (one per attempt, the last repeating),
/// returning the settled message, the attempt count, the waited delays and
/// the events.
async fn run_attempts(
    policy: &ProviderRetryPolicy,
    outcomes: Vec<AssistantMessage>,
) -> (AssistantMessage, usize, Vec<u64>, Vec<AutoRetryEvent>) {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let waits = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let (attempts_seen, waits_seen, events_seen) = (
        Arc::clone(&attempts),
        Arc::clone(&waits),
        Arc::clone(&events),
    );
    let message = run_turn_with_auto_retry(
        policy,
        0,
        None,
        || {
            let index = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let outcome = outcomes[index.min(outcomes.len() - 1)].clone();
            async move { Ok(outcome) }
        },
        move |event| {
            let events = Arc::clone(&events);
            async move {
                events.lock().unwrap().push(event);
                Ok(())
            }
        },
        move |delay| {
            waits
                .lock()
                .unwrap()
                .push(u64::try_from(delay.as_millis()).unwrap());
            async { true }
        },
        None,
    )
    .await
    .unwrap();
    let waits = waits_seen.lock().unwrap().clone();
    let events = events_seen.lock().unwrap().clone();
    (
        message,
        attempts_seen.load(std::sync::atomic::Ordering::SeqCst),
        waits,
        events,
    )
}

/// A network outage outlasts the quick ladder: connection failures keep
/// retrying past `max_retries` until the waits spent in the outage reach
/// `connection_wait_ms`, then the loop gives up with the last failure.
#[tokio::test]
async fn connection_failures_retry_past_the_ladder_until_the_outage_budget_is_spent() {
    let policy = outage_policy(1_000);
    let (message, attempts, waits, events) =
        run_attempts(&policy, vec![connection_failure_message()]).await;
    assert_eq!(message.error_message.as_deref(), Some("Connection error."));
    let waited: u64 = waits.iter().sum();
    let before_last = waited - waits.last().copied().unwrap_or(0);
    assert!(
        attempts > 1 + policy.max_retries as usize,
        "the outage outlasts the ladder: {attempts} attempts"
    );
    assert!(
        before_last < 1_000 && waited >= 1_000,
        "retries until the waits reach the budget: {waits:?}"
    );
    assert_eq!(attempts, waits.len() + 1);
    let retries = u32::try_from(waits.len()).unwrap();
    assert_eq!(
        events.last(),
        Some(&AutoRetryEvent::End {
            success: false,
            attempt: retries,
            final_error: Some("Connection error.".to_string()),
            restored_model: None,
        })
    );
    // Each start counts against a total that grows with the outage.
    assert!(events.iter().all(|event| match event {
        AutoRetryEvent::Start {
            attempt,
            max_attempts,
            ..
        } => attempt <= max_attempts,
        AutoRetryEvent::End { .. } => true,
    }));
}

/// The network comes back mid-outage: the turn resumes and settles as a
/// successful retry, past the quick ladder's count.
#[tokio::test]
async fn a_turn_resumes_when_the_network_returns_mid_outage() {
    let policy = outage_policy(60_000);
    let mut outcomes = vec![connection_failure_message(); 6];
    outcomes.push(ok_message());
    let (message, attempts, _waits, events) = run_attempts(&policy, outcomes).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(attempts, 7);
    assert_eq!(
        events.last(),
        Some(&AutoRetryEvent::End {
            success: true,
            attempt: 6,
            final_error: None,
            restored_model: None,
        })
    );
}

/// The outage budget is for connection failures only: a server error keeps
/// the quick ladder.
#[tokio::test]
async fn failures_with_a_response_keep_the_quick_ladder() {
    let policy = outage_policy(60_000);
    let (_message, attempts, _waits, _events) = run_attempts(
        &policy,
        vec![error_message(Some("server_error"), None, None)],
    )
    .await;
    assert_eq!(attempts, 1 + policy.max_retries as usize);
}

/// Outage waits stay short, so the turn lands soon after the network
/// returns: past the ladder each wait is capped (before jitter).
#[tokio::test]
async fn outage_waits_are_capped() {
    let policy = ProviderRetryPolicy {
        max_retries: 0,
        base_delay_ms: 60_000,
        ..outage_policy(40_000)
    };
    let (_message, _attempts, waits, _events) =
        run_attempts(&policy, vec![connection_failure_message()]).await;
    let ceiling = CONNECTION_RETRY_MAX_DELAY_MS + CONNECTION_RETRY_MAX_DELAY_MS / 5;
    assert!(!waits.is_empty());
    assert!(waits.iter().all(|wait| *wait <= ceiling), "{waits:?}");
}
