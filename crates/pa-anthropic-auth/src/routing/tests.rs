//! The plugins' routing over a temporary store, a temporary sidecar, a
//! loopback usage endpoint and a mock Messages endpoint.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use anthropic::AccountStore;
use chrono::Duration;
use pa_core::auth::ProviderCredentialSource;
use pa_types::sync::MutexExt;
use serde_json::json;

use crate::test_support::*;
use crate::SharedStoreSource;

const RATE_LIMITED: &str =
    r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}"#;
const FAR: &str = "2099-01-01T00:00:00Z";

/// A store of `ids` (the first pinned), a sidecar holding `sidecar`, the
/// sticky state beside it, and a usage endpoint answering from `bodies`.
struct Fixture {
    _home: tempfile::TempDir,
    source: Arc<SharedStoreSource>,
    bodies: UsageBodies,
}

fn fixture(provider: &str, ids: &[&str], sidecar_document: &serde_json::Value) -> Fixture {
    let bodies = UsageBodies::default();
    let (usage_url, _polls) = usage_endpoint(Arc::clone(&bodies));
    let dir = tempfile::tempdir().expect("a temporary dir");
    let config_path = sidecar(dir.path(), sidecar_document);
    let rows = ids.iter().map(|id| row(id, Duration::hours(2))).collect();
    let (home, source) = source_configured(rows, |config| {
        config.endpoints.usage_url = usage_url.clone();
        config.config_path = Some(config_path);
    });
    std::mem::forget(dir);
    AccountStore::mutate(source.store_path(), |store| store.set_current(ids[0]))
        .expect("pin the first login");
    pa_core::auth::install_credential_source(provider, source.clone());
    pa_ai::request_hooks::install_request_hooks(provider, source.clone());
    Fixture {
        _home: home,
        source,
        bodies,
    }
}

impl Fixture {
    fn usage(&self, id: &str, body: String) {
        self.bodies.lock_or_recover().insert(access_of(id), body);
    }

    /// Send `replies.len()` requests for `model_id`; the bearers the
    /// Messages endpoint saw, and the last message.
    fn send(
        &self,
        provider: &str,
        model_id: &str,
        replies: Vec<Reply>,
        requests: usize,
    ) -> (Vec<String>, pa_ai::types::AssistantMessage) {
        let (base, seen) = messages_endpoint(replies);
        let mut model = messages_model(provider, &base);
        model.id = model_id.to_string();
        let served = self.source.credential().expect("a login").api_key;
        let mut last = None;
        for _ in 0..requests {
            last = Some(complete(&model, &served));
        }
        let bearers = seen
            .lock_or_recover()
            .iter()
            .map(CapturedRequest::bearer)
            .collect();
        (bearers, last.expect("one request"))
    }
}

fn ok(count: usize) -> Vec<Reply> {
    vec![(200, Vec::new(), OK_STREAM); count]
}

#[test]
fn main_first_passes_over_a_login_whose_model_window_is_spent() {
    let provider = "anthropic-route-main-first";
    let fixture = fixture(provider, &["first", "second"], &json!({ "accounts": [] }));
    fixture.usage("first", usage_body(10.0, 10.0, FAR, Some(100.0)));
    fixture.usage("second", usage_body(10.0, 10.0, FAR, Some(20.0)));

    let (bearers, _) = fixture.send(provider, "claude-fable-5", ok(2), 2);

    // The first request learns (polls) that the first login's Fable window
    // is spent; the second goes to the login with Fable headroom.
    assert_eq!(bearers, vec![access_of("first"), access_of("second")]);
    assert_eq!(fixture.source.counts.quota_routed.load(Ordering::SeqCst), 1);
}

#[test]
fn fallback_first_prefers_another_login_that_passes_the_policy() {
    let provider = "anthropic-route-fallback-first";
    let fixture = fixture(
        provider,
        &["main", "spare"],
        &json!({ "accounts": [], "routing": { "mode": "fallback-first" } }),
    );
    fixture.usage("spare", usage_body(10.0, 10.0, FAR, None));

    let (bearers, _) = fixture.send(provider, "claude-opus-5-5", ok(1), 1);

    assert_eq!(bearers, vec![access_of("spare")]);
}

#[test]
fn a_login_under_its_minimum_remaining_is_no_fallback() {
    let provider = "anthropic-route-minimum";
    let fixture = fixture(
        provider,
        &["main", "low"],
        &json!({
            "accounts": [],
            "routing": { "mode": "fallback-first" },
            "quota": { "minimumRemaining": { "5h": 50 } }
        }),
    );
    // 40% left in the 5h window, under the 50% minimum.
    fixture.usage("low", usage_body(60.0, 10.0, FAR, None));

    let (bearers, _) = fixture.send(provider, "claude-opus-5-5", ok(1), 1);

    assert_eq!(bearers, vec![access_of("main")]);
}

#[test]
fn the_killswitch_moves_past_a_login_below_its_threshold() {
    let provider = "anthropic-route-killswitch";
    let fixture = fixture(
        provider,
        &["killed", "alive"],
        &json!({ "accounts": [], "killswitch": { "enabled": true } }),
    );
    // 3% left in the 5h window: under the default 5%.
    fixture.usage("killed", usage_body(97.0, 10.0, FAR, None));
    fixture.usage("alive", usage_body(10.0, 10.0, FAR, None));

    let (bearers, _) = fixture.send(provider, "claude-opus-5-5", ok(1), 1);

    assert_eq!(bearers, vec![access_of("alive")]);
}

#[test]
fn a_per_login_threshold_overrides_the_main_one() {
    let provider = "anthropic-route-killswitch-override";
    let fixture = fixture(
        provider,
        &["lenient"],
        &json!({
            "accounts": [],
            "killswitch": { "enabled": true, "accounts": { "lenient": { "5h": 1, "1w": 1 } } }
        }),
    );
    fixture.usage("lenient", usage_body(97.0, 10.0, FAR, None));

    let (bearers, _) = fixture.send(provider, "claude-opus-5-5", ok(1), 1);

    assert_eq!(bearers, vec![access_of("lenient")]);
}

#[test]
fn the_killswitch_refuses_a_request_no_login_may_serve() {
    let provider = "anthropic-route-killswitch-block";
    let fixture = fixture(
        provider,
        &["only"],
        &json!({ "accounts": [], "killswitch": { "enabled": true } }),
    );
    fixture.usage("only", usage_body(97.0, 10.0, FAR, None));

    let (bearers, message) = fixture.send(provider, "claude-opus-5-5", ok(1), 1);

    assert_eq!(bearers, Vec::<String>::new());
    assert_eq!(message.stop_reason, pa_ai::types::StopReason::Error);
    let error = message.error_message.unwrap_or_default();
    assert!(
        error.contains("Killswitch: no routable accounts. Retry in "),
        "{error}"
    );
    assert_eq!(fixture.source.counts.blocked.load(Ordering::SeqCst), 1);
}

#[test]
fn a_scoped_killswitch_block_names_the_model_s_weekly_limit() {
    let provider = "anthropic-route-killswitch-scoped";
    let fixture = fixture(
        provider,
        &["fable"],
        &json!({ "accounts": [], "killswitch": { "enabled": true } }),
    );
    fixture.usage("fable", usage_body(10.0, 10.0, FAR, Some(100.0)));

    let (bearers, message) = fixture.send(provider, "claude-fable-5", ok(1), 1);

    assert_eq!(bearers, Vec::<String>::new());
    let error = message.error_message.unwrap_or_default();
    assert!(
        error.contains("Fable weekly limit reached, no routable accounts. Retry in "),
        "{error}"
    );
}

#[test]
fn after_a_429_a_login_of_unknown_quota_takes_the_request_only_failing_open() {
    for (fail_closed, expected) in [
        (true, vec![access_of("limited-c")]),
        (false, vec![access_of("limited-o"), access_of("unknown-o")]),
    ] {
        let suffix = if fail_closed { "c" } else { "o" };
        let provider = format!("anthropic-route-429-unknown-{suffix}");
        let limited = format!("limited-{suffix}");
        let unknown = format!("unknown-{suffix}");
        let fixture = fixture(
            &provider,
            &[&limited, &unknown],
            &json!({ "accounts": [], "quota": { "failClosedOnUnknownQuota": fail_closed } }),
        );
        fixture.usage(&limited, usage_body(10.0, 10.0, FAR, None));
        // The other login's usage poll fails: its quota stays unknown.

        let (bearers, _) = fixture.send(
            &provider,
            "claude-opus-5-5",
            vec![
                (429, vec![("retry-after", "30".to_string())], RATE_LIMITED),
                (200, Vec::new(), OK_STREAM),
            ],
            1,
        );

        assert_eq!(bearers, expected, "fail closed: {fail_closed}");
    }
}
