//! The plugins' routing over a temporary store, a temporary sidecar, a
//! loopback usage endpoint and a mock Messages endpoint.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use anthropic::AccountStore;
use chrono::{Duration, Utc};
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
    /// The sidecar and sticky-state dir, removed with the fixture.
    _sidecar_dir: tempfile::TempDir,
    source: Arc<SharedStoreSource>,
    bodies: UsageBodies,
    polls: Arc<std::sync::atomic::AtomicUsize>,
    state: std::path::PathBuf,
}

fn fixture(provider: &str, ids: &[&str], sidecar_document: &serde_json::Value) -> Fixture {
    let bodies = UsageBodies::default();
    let (usage_url, polls) = usage_endpoint(Arc::clone(&bodies));
    let dir = tempfile::tempdir().expect("a temporary dir");
    let config_path = sidecar(dir.path(), sidecar_document);
    let state = dir.path().join("anthropic-auth-routing-state.json");
    let rows = ids.iter().map(|id| row(id, Duration::hours(2))).collect();
    let (home, source) = source_configured(rows, |config| {
        config.endpoints.usage_url = usage_url.clone();
        config.config_path = Some(config_path);
        config.routing_state_path = Some(state.clone());
    });
    AccountStore::mutate(source.store_path(), |store| store.set_current(ids[0]))
        .expect("pin the first login");
    pa_core::auth::install_credential_source(provider, source.clone());
    pa_ai::request_hooks::install_request_hooks(provider, source.clone());
    Fixture {
        _home: home,
        _sidecar_dir: dir,
        source,
        bodies,
        polls,
        state,
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

fn sticky(provider: &str, ids: &[&str], quota: &serde_json::Value) -> Fixture {
    let fixture = fixture(
        provider,
        ids,
        &json!({ "accounts": [], "routing": { "mode": "sticky-balanced" }, "quota": quota }),
    );
    *fixture.source.session.lock_or_recover() = Some(format!("{provider}-session"));
    fixture
}

#[test]
fn a_sticky_session_is_assigned_by_headroom_and_kept() {
    let provider = "anthropic-route-sticky";
    let fixture = sticky(provider, &["busy", "roomy"], &json!({}));
    fixture.usage("busy", usage_body(80.0, 10.0, FAR, None));
    fixture.usage("roomy", usage_body(10.0, 10.0, FAR, None));

    let (bearers, _) = fixture.send(provider, "claude-opus-5-5", ok(2), 2);

    assert_eq!(bearers, vec![access_of("roomy"), access_of("roomy")]);
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&fixture.state).expect("the sticky state"))
            .expect("JSON");
    let assignments = state["assignments"].as_object().expect("assignments");
    assert_eq!(
        assignments
            .values()
            .map(|assignment| assignment["accountId"].clone())
            .collect::<Vec<_>>(),
        vec![json!("roomy")]
    );
    // The session id is stored hashed, never as is.
    assert!(!assignments.contains_key(&format!("{provider}-session")));
    assert_eq!(
        fixture.source.counts.sticky_assigned.load(Ordering::SeqCst),
        1
    );
    // Both logins polled once to assign; nothing more while fresh.
    assert_eq!(fixture.polls.load(Ordering::SeqCst), 2);
}

#[test]
fn an_unconfirmed_429_keeps_the_sticky_session_on_its_login() {
    let provider = "anthropic-route-sticky-retain";
    let fixture = sticky(provider, &["kept", "other"], &json!({}));
    fixture.usage("kept", usage_body(10.0, 10.0, FAR, None));
    fixture.usage("other", usage_body(50.0, 50.0, FAR, None));

    let (bearers, message) = fixture.send(
        provider,
        "claude-opus-5-5",
        vec![(429, vec![("retry-after", "30".to_string())], RATE_LIMITED)],
        1,
    );

    assert_eq!(bearers, vec![access_of("kept")]);
    assert_eq!(message.stop_reason, pa_ai::types::StopReason::Error);
    // No cooldown: the session stays where it is.
    let store = AccountStore::load(fixture.source.store_path()).expect("the store");
    assert_eq!(
        store
            .get("kept")
            .and_then(|account| account.rate_limited_until),
        None
    );
}

#[test]
fn a_confirmed_exhaustion_moves_the_sticky_session() {
    let provider = "anthropic-route-sticky-migrate";
    let fixture = sticky(provider, &["spent", "next"], &json!({}));
    fixture.usage("spent", usage_body(10.0, 10.0, FAR, None));
    fixture.usage("next", usage_body(50.0, 50.0, FAR, None));
    let (base, seen) = messages_endpoint(vec![
        (200, Vec::new(), OK_STREAM),
        (429, Vec::new(), RATE_LIMITED),
        (200, Vec::new(), OK_STREAM),
    ]);
    let model = messages_model(provider, &base);
    let served = fixture.source.credential().expect("a login").api_key;

    complete(&model, &served);
    // The login's week runs out: the poll after the 429 confirms it.
    fixture.usage("spent", usage_body(10.0, 100.0, FAR, None));
    let message = complete(&model, &served);

    assert_eq!(text_of(&message), "hello");
    assert_eq!(
        seen.lock_or_recover()
            .iter()
            .map(CapturedRequest::bearer)
            .collect::<Vec<_>>(),
        vec![access_of("spent"), access_of("spent"), access_of("next")]
    );
    assert_eq!(
        fixture.source.counts.sticky_migrated.load(Ordering::SeqCst),
        1
    );
}

#[test]
fn a_sticky_pool_with_no_eligible_login_is_refused() {
    let provider = "anthropic-route-sticky-none";
    let fixture = sticky(
        provider,
        &["low", "lower"],
        &json!({ "minimumRemaining": { "5h": 50 } }),
    );
    fixture.usage("low", usage_body(60.0, 10.0, FAR, None));
    fixture.usage("lower", usage_body(70.0, 10.0, FAR, None));

    let (bearers, message) = fixture.send(provider, "claude-opus-5-5", ok(1), 1);

    assert_eq!(bearers, Vec::<String>::new());
    let error = message.error_message.unwrap_or_default();
    assert!(
        error.contains(
            "No OAuth account currently satisfies sticky-balanced quota policy. Retry in "
        ),
        "{error}"
    );
}

#[test]
fn a_sticky_login_whose_5h_window_resets_shortly_holds_the_session() {
    use sha2::Digest;
    let provider = "anthropic-route-sticky-hold";
    let fixture = sticky(provider, &["held", "spare"], &json!({}));
    // The session sits on `held` (as an earlier process assigned it).
    let now = Utc::now().timestamp_millis();
    let key = sha2::Sha256::digest(format!("{provider}-session").as_bytes())
        .iter()
        .fold(String::new(), |mut key, byte| {
            use std::fmt::Write;
            let _ = write!(key, "{byte:02x}");
            key
        });
    let state = json!({ "version": 1, "updatedAt": now, "assignments": { key: {
        "accountId": "held", "family": "opus", "affinityModelId": "claude-opus-5-5",
        "assignedAt": now, "lastSeenAt": now, "initialInputBytes": 10, "quotaCheckedAt": now
    } } });
    std::fs::write(&fixture.state, state.to_string()).expect("seed the sticky state");
    // Its 5h window is spent and resets in ten minutes.
    let soon = (Utc::now() + Duration::minutes(10)).to_rfc3339();
    fixture.usage("held", usage_body(100.0, 10.0, &soon, None));
    fixture.usage("spare", usage_body(10.0, 10.0, FAR, None));

    let (bearers, message) = fixture.send(provider, "claude-opus-5-5", ok(1), 1);

    assert_eq!(bearers, Vec::<String>::new());
    let error = message.error_message.unwrap_or_default();
    assert!(
        error.contains("Sticky OAuth account five-hour quota resets shortly"),
        "{error}"
    );
}

#[test]
fn the_session_feature_names_the_sticky_session() {
    let (_home, source) = source_over(
        vec![row("session", Duration::hours(2))],
        "http://127.0.0.1:9",
    );
    let feature = crate::AnthropicAuthFeature::new(Arc::clone(&source));
    let context = |session: &str, depth: u32| {
        Arc::new(pa_core::features::SessionFeatureContext {
            agent_dir: std::path::PathBuf::from("/nonexistent/agent"),
            cwd: std::path::PathBuf::from("/nonexistent/cwd"),
            session_id: session.to_string(),
            python_skill_import_names: Vec::new(),
            model: serde_json::from_value(json!({
                "id": "claude-opus-5-5", "name": "Claude Opus 5.5", "api": "anthropic-messages",
                "provider": "anthropic", "baseUrl": "http://localhost", "reasoning": true,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                "contextWindow": 1000, "maxTokens": 100
            }))
            .expect("stub model"),
            telemetry: None,
            rlm_depth: depth,
            session_artifact_dir: None,
        })
    };
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();

    for (session, depth) in [("parent", 0), ("child", 1), ("next", 0)] {
        pa_core::features::SessionFeature::on_session_start(
            &feature,
            &context(session, depth),
            &[],
        );
        seen.lock_or_recover().push(source.session());
    }

    assert_eq!(
        *seen.lock_or_recover(),
        vec![
            Some("parent".to_string()),
            Some("parent".to_string()),
            Some("next".to_string())
        ]
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

#[test]
fn another_process_s_reading_never_erases_what_a_poll_learned() {
    let provider = "anthropic-route-seed";
    let fixture = fixture(provider, &["seeded"], &json!({ "accounts": [] }));
    fixture.usage("seeded", usage_body(10.0, 10.0, FAR, Some(20.0)));
    let (_bearers, _) = fixture.send(provider, "claude-opus-5-5", ok(1), 1);
    // Another tool records a newer coarse reading on the row.
    AccountStore::mutate(fixture.source.store_path(), |store| {
        store.get_mut("seeded")?.quota = Some(anthropic::account::QuotaObservation {
            five_hour_percent: Some(40.0),
            seven_day_percent: Some(40.0),
            checked_at: Some(chrono::Utc::now() + Duration::seconds(5)),
        });
        Ok(())
    })
    .expect("record another reading");

    let (_bearers, _) = fixture.send(provider, "claude-opus-5-5", ok(1), 1);

    let known = fixture.source.quota.snapshot("seeded").expect("a reading");
    assert_eq!(
        (
            known.scoped.map(|scoped| scoped.len()),
            known.five_hour.and_then(|window| window.resets_at)
        ),
        (Some(1), Some(FAR.to_string()))
    );
}

#[test]
fn a_new_sticky_assignment_prefers_the_login_the_session_s_cache_is_kept_on() {
    let provider = "anthropic-route-sticky-cachekeep";
    let fixture = sticky(provider, &["roomy-cold", "busy-warm"], &json!({}));
    fixture.usage("roomy-cold", usage_body(10.0, 10.0, FAR, None));
    fixture.usage("busy-warm", usage_body(80.0, 10.0, FAR, None));
    // The session's cache is kept warm on the busier login (an earlier
    // request of this process went there).
    fixture
        .source
        .cachekeep
        .track(
            &crate::cachekeep::Track {
                session_id: Some(&format!("{provider}-session")),
                url: "http://127.0.0.1:9/v1/messages",
                headers: &[],
                body_text: "{}",
                account_id: "busy-warm",
            },
            &crate::cachekeep::Settings {
                enabled: true,
                always: true,
                window: None,
                hybrid_cache: true,
            },
            &Utc::now().fixed_offset(),
        )
        .expect("tracked");

    let (bearers, _) = fixture.send(provider, "claude-opus-5-5", ok(1), 1);

    assert_eq!(bearers, vec![access_of("busy-warm")]);
}
