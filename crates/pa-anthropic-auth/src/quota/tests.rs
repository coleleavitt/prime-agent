//! Quota, the quota reserve and the 429 rotation against a temporary store
//! and a mock Messages endpoint.

use std::sync::{Arc, Mutex};

use anthropic::AccountStore;
use anthropic::account::QuotaObservation;
use chrono::{Duration, Utc};
use pa_core::auth::ProviderCredentialSource;
use pa_core::features::{FeatureStatus, SessionFeature, SessionFeatureContext};
use pa_types::sync::MutexExt;

use super::*;
use crate::test_support::*;
use crate::{SharedStoreConfig, SharedStoreSource};

const RATE_LIMITED: &str =
    r#"{"type":"error","error":{"type":"rate_limit_error","message":"rate limited"}}"#;

fn install(provider: &str, source: &Arc<SharedStoreSource>) {
    pa_core::auth::install_credential_source(provider, source.clone());
    pa_ai::request_hooks::install_request_hooks(provider, source.clone());
}

fn pin(source: &SharedStoreSource, id: &str) {
    AccountStore::mutate(source.store_path(), |store| store.set_current(id)).expect("pin a login");
}

fn stored(source: &SharedStoreSource) -> AccountStore {
    AccountStore::load(source.store_path()).expect("the store")
}

fn bearers(requests: &Mutex<Vec<CapturedRequest>>) -> Vec<String> {
    requests
        .lock_or_recover()
        .iter()
        .map(CapturedRequest::bearer)
        .collect()
}

#[test]
fn a_429_moves_to_the_next_login_in_the_store_s_order() {
    let provider = "anthropic-quota-429";
    let (usage_url, _usage_hits) = token_endpoint(200, USAGE);
    let (_home, source) = source_configured(
        vec![
            row("limited", Duration::hours(2)),
            row("next", Duration::hours(2)),
        ],
        |config| config.endpoints.usage_url = usage_url.clone(),
    );
    pin(&source, "limited");
    install(provider, &source);
    let (base, requests) = messages_endpoint(vec![
        (429, vec![("retry-after", "120".to_string())], RATE_LIMITED),
        (200, Vec::new(), OK_STREAM),
    ]);
    let served = source.credential().expect("the pinned login").api_key;
    let before = Utc::now();

    let message = complete(&messages_model(provider, &base), &served);

    assert_eq!(text_of(&message), "hello");
    assert_eq!(
        bearers(&requests),
        vec![
            "sk-ant-oat01-limited-store-access-000".to_string(),
            "sk-ant-oat01-next-store-access-000".to_string()
        ]
    );
    let store = stored(&source);
    // Cooling down for the server's retry-after, and unpinned.
    let until = store
        .get("limited")
        .and_then(|account| account.rate_limited_until)
        .expect("a cooldown");
    assert!(
        until >= before + Duration::seconds(119) && until <= Utc::now() + Duration::seconds(121)
    );
    assert_eq!(store.current, None);
    assert_eq!(source.usage().rotated, 1);
    // The next request goes to the next login straight away.
    assert_eq!(
        source.credential().map(|credential| credential.api_key),
        Ok("sk-ant-oat01-next-store-access-000".to_string())
    );
}

#[test]
fn a_429_with_no_other_login_is_reported_and_the_login_keeps_serving() {
    let provider = "anthropic-quota-429-alone";
    let (_home, source) = source_over(vec![row("alone", Duration::hours(2))], "http://127.0.0.1:9");
    install(provider, &source);
    let (base, requests) = messages_endpoint(vec![(
        429,
        vec![("retry-after", "30".to_string())],
        RATE_LIMITED,
    )]);
    let served = source.credential().expect("the login").api_key;

    let message = complete(&messages_model(provider, &base), &served);

    assert_eq!(message.stop_reason, pa_ai::types::StopReason::Error);
    assert_eq!(bearers(&requests).len(), 1);
    assert!(
        stored(&source)
            .get("alone")
            .and_then(|account| account.rate_limited_until)
            .is_some()
    );
    // Still a login (not "no API key"), and its live token still serves:
    // the provider's answer decides, as it does for the plugins.
    assert!(source.status().is_some());
    assert_eq!(
        source.credential().map(|credential| credential.api_key),
        Ok(served)
    );
}

#[test]
fn responses_record_the_quota_on_the_row_and_mark_it_used() {
    let provider = "anthropic-quota-headers";
    let (_home, source) = source_over(
        vec![row("reading", Duration::hours(2))],
        "http://127.0.0.1:9",
    );
    install(provider, &source);
    let (base, _requests) = messages_endpoint(vec![(
        200,
        vec![
            (
                "anthropic-ratelimit-unified-5h-utilization",
                "0.48".to_string(),
            ),
            (
                "anthropic-ratelimit-unified-7d-utilization",
                "0.553".to_string(),
            ),
        ],
        OK_STREAM,
    )]);
    let served = source.credential().expect("the login").api_key;

    complete(&messages_model(provider, &base), &served);

    let account = stored(&source).get("reading").cloned().expect("the row");
    assert_eq!(
        account
            .quota
            .map(|quota| (quota.five_hour_percent, quota.seven_day_percent)),
        Some((Some(48.0), Some(55.0)))
    );
    assert!(account.last_used_at.is_some());
    assert_eq!(
        source.quota_line().map(|quota| quota.line),
        Some("Claude quota: 5h 48% / 7d 55% used".to_string())
    );
}

fn context(session: &str) -> Arc<SessionFeatureContext> {
    Arc::new(SessionFeatureContext {
        agent_dir: std::path::PathBuf::from("/nonexistent/agent"),
        cwd: std::path::PathBuf::from("/nonexistent/cwd"),
        session_id: session.to_string(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(serde_json::json!({
            "id": "claude-opus-5-5", "name": "Claude Opus 5.5", "api": "anthropic-messages",
            "provider": "anthropic", "baseUrl": "http://localhost", "reasoning": true,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .expect("stub model"),
        telemetry: None,
        rlm_depth: 0,
        session_artifact_dir: None,
    })
}

#[test]
fn an_anthropic_session_shows_the_store_s_quota_once_per_change() {
    let mut recorded = row("shown", Duration::hours(2));
    recorded.quota = Some(QuotaObservation {
        five_hour_percent: Some(12.4),
        seven_day_percent: Some(70.0),
        checked_at: Some(Utc::now()),
    });
    let (_home, source) = source_over(vec![recorded], "http://127.0.0.1:9");
    let session = "quota-session";
    let statuses: Arc<Mutex<Vec<FeatureStatus>>> = Arc::default();
    let sink_statuses = Arc::clone(&statuses);
    let sink: pa_core::features::FeatureStatusSink =
        Arc::new(move |status| sink_statuses.lock_or_recover().push(status));
    pa_core::features::register_feature_status_sink(session, &sink);
    let feature = crate::AnthropicAuthFeature::new(Arc::clone(&source));

    // Nothing served yet: nothing to show.
    feature.on_agent_end(&context(session));
    source.credential().expect("the login");
    feature.on_agent_end(&context(session));
    feature.on_agent_end(&context(session));

    let shown = statuses.lock_or_recover().clone();
    assert_eq!(shown.len(), 1);
    assert_eq!(
        (shown[0].feature.as_str(), shown[0].line.as_deref()),
        ("anthropic-auth", Some("Claude quota: 5h 12% / 7d 70% used"))
    );
    assert_eq!(shown[0].status["source"], "store");
    drop(sink);
}

fn reading(five: f64) -> QuotaObservation {
    QuotaObservation {
        five_hour_percent: Some(five),
        seven_day_percent: Some(10.0),
        checked_at: Some(Utc::now()),
    }
}

#[test]
fn the_quota_reserve_prefers_a_login_under_it() {
    let mut spent = row("reserved", Duration::hours(2));
    spent.quota = Some(reading(95.0));
    let mut fresh = row("headroom", Duration::hours(2));
    fresh.quota = Some(reading(20.0));
    let (_home, seeded) = source_over(vec![spent, fresh], "http://127.0.0.1:9");
    pin(&seeded, "reserved");
    let mut config = SharedStoreConfig::isolated(
        seeded.store_path().to_path_buf(),
        "http://127.0.0.1:9/v1/oauth/token",
        "http://127.0.0.1:9/api/oauth/profile",
    );
    config.quota_reserve = Some(90.0);
    let source = SharedStoreSource::new(config.clone());

    assert_eq!(
        source.credential().map(|credential| credential.api_key),
        Ok("sk-ant-oat01-headroom-store-access-000".to_string())
    );

    // Every login at the reserve: the store's own pick still serves.
    AccountStore::mutate(seeded.store_path(), |store| {
        store.get_mut("headroom")?.quota = Some(reading(96.0));
        Ok(())
    })
    .expect("spend the other login");
    let source = SharedStoreSource::new(config);
    assert_eq!(
        source.credential().map(|credential| credential.api_key),
        Ok("sk-ant-oat01-reserved-store-access-000".to_string())
    );
}

#[test]
fn the_cooldown_follows_the_server() {
    let now = Utc::now();
    let headers = |pairs: &[(&str, String)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect()
    };
    let five_hour_reset = (now + Duration::minutes(90)).timestamp();
    let seven_day_reset = (now + Duration::days(2)).timestamp();

    assert_eq!(
        cooldown_until(&headers(&[("retry-after", "45".to_string())]), now),
        now + Duration::seconds(45)
    );
    assert_eq!(
        cooldown_until(
            &headers(&[
                (
                    "anthropic-ratelimit-unified-representative-claim",
                    "five_hour".to_string()
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    five_hour_reset.to_string()
                ),
                (
                    "anthropic-ratelimit-unified-7d-reset",
                    seven_day_reset.to_string()
                ),
            ]),
            now
        )
        .timestamp(),
        five_hour_reset
    );
    assert_eq!(
        cooldown_until(
            &headers(&[
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    five_hour_reset.to_string()
                ),
                (
                    "anthropic-ratelimit-unified-7d-reset",
                    seven_day_reset.to_string()
                ),
            ]),
            now
        )
        .timestamp(),
        seven_day_reset
    );
    assert_eq!(cooldown_until(&[], now), now + Duration::seconds(60));
}

fn usage_at(url: &str) -> impl FnOnce(&mut SharedStoreConfig) + '_ {
    move |config| config.endpoints.usage_url = url.to_string()
}

fn recorded(source: &SharedStoreSource, id: &str) -> Option<(Option<f64>, Option<f64>)> {
    stored(source)
        .get(id)
        .and_then(|account| account.quota.clone())
        .map(|quota| (quota.five_hour_percent, quota.seven_day_percent))
}

#[test]
fn a_due_login_is_polled_and_the_reading_lands_on_its_row() {
    let provider = "anthropic-quota-poll";
    let (usage_base, usage_requests) = messages_endpoint(vec![(200, Vec::new(), USAGE)]);
    let usage_url = format!("{usage_base}/api/oauth/usage");
    let (_home, source) = source_configured(
        vec![row("polled", Duration::hours(2))],
        usage_at(&usage_url),
    );
    install(provider, &source);
    let (base, _requests) = messages_endpoint(vec![(200, Vec::new(), OK_STREAM)]);
    let served = source.credential().expect("the login").api_key;

    complete(&messages_model(provider, &base), &served);

    let polls = usage_requests.lock_or_recover().clone();
    assert_eq!(
        polls
            .iter()
            .map(|poll| (
                poll.bearer(),
                poll.header("anthropic-beta").map(str::to_string)
            ))
            .collect::<Vec<_>>(),
        vec![(served, Some("oauth-2025-04-20".to_string()))]
    );
    assert_eq!(recorded(&source, "polled"), Some((Some(30.0), Some(60.0))));
    let line = source.quota_line().expect("a quota line");
    assert_eq!(
        (line.line.as_str(), &line.status["source"]),
        (
            "Claude quota: 5h 30% / 7d 60% used",
            &serde_json::json!("poll")
        )
    );
    assert_eq!(source.quota.poll_counts(), (1, 0));
}

#[test]
fn a_fresh_reading_waits_for_its_interval_and_every_n_requests_forces_a_poll() {
    let provider = "anthropic-quota-poll-cadence";
    let (usage_url, usage_hits) = token_endpoint(200, USAGE);
    let dir = tempfile::tempdir().expect("a temporary dir");
    let config_path = sidecar(
        dir.path(),
        &serde_json::json!({ "accounts": [], "quota": { "refreshEveryNRequests": 3 } }),
    );
    let (_home, source) = source_configured(vec![row("cadence", Duration::hours(2))], |config| {
        config.endpoints.usage_url = usage_url.clone();
        config.config_path = Some(config_path);
    });
    install(provider, &source);
    let (base, _requests) = messages_endpoint(vec![(200, Vec::new(), OK_STREAM); 4]);
    let served = source.credential().expect("the login").api_key;
    let model = messages_model(provider, &base);

    let mut hits = Vec::new();
    for _ in 0..4 {
        complete(&model, &served);
        hits.push(usage_hits.load(std::sync::atomic::Ordering::SeqCst));
    }

    // The first request finds nothing known; the second a fresh reading;
    // the third is the third request; the fourth a fresh reading again.
    assert_eq!(hits, vec![1, 1, 2, 2]);
}

#[test]
fn a_failed_poll_backs_off() {
    let provider = "anthropic-quota-poll-backoff";
    let (usage_url, usage_hits) = token_endpoint(500, r#"{"error":"busy"}"#);
    let (_home, source) = source_configured(
        vec![row("backoff", Duration::hours(2))],
        usage_at(&usage_url),
    );
    install(provider, &source);
    let (base, _requests) = messages_endpoint(vec![(200, Vec::new(), OK_STREAM); 3]);
    let served = source.credential().expect("the login").api_key;
    let model = messages_model(provider, &base);

    for _ in 0..3 {
        complete(&model, &served);
    }

    assert_eq!(usage_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(source.quota.poll_counts(), (1, 1));
    assert_eq!(recorded(&source, "backoff"), None);
}

#[test]
fn a_newer_header_reading_wins_over_the_poll_and_keeps_its_scoped_windows() {
    let provider = "anthropic-quota-poll-merge";
    let (usage_url, _usage_hits) = token_endpoint(200, USAGE);
    let (_home, source) = source_configured(
        vec![row("merged", Duration::hours(2))],
        usage_at(&usage_url),
    );
    install(provider, &source);
    let (base, _requests) = messages_endpoint(vec![
        (200, Vec::new(), OK_STREAM),
        (
            200,
            vec![
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.48".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-7d-utilization",
                    "0.553".to_string(),
                ),
            ],
            OK_STREAM,
        ),
    ]);
    let served = source.credential().expect("the login").api_key;
    let model = messages_model(provider, &base);

    complete(&model, &served);
    complete(&model, &served);

    let line = source.quota_line().expect("a quota line");
    assert_eq!(
        (line.line.as_str(), &line.status["source"]),
        (
            "Claude quota: 5h 48% / 7d 55% used",
            &serde_json::json!("headers")
        )
    );
    assert_eq!(
        source
            .quota
            .snapshot("merged")
            .and_then(|quota| quota.scoped)
            .map(|scoped| scoped
                .into_iter()
                .map(|window| (window.model_name, window.remaining_percent))
                .collect::<Vec<_>>()),
        Some(vec![("Fable".to_string(), 75.0)])
    );
    assert_eq!(recorded(&source, "merged"), Some((Some(48.0), Some(55.0))));
}

#[test]
fn a_429_is_confirmed_by_a_poll_before_the_request_moves_on() {
    let provider = "anthropic-quota-429-poll";
    let (usage_url, usage_hits) = token_endpoint(200, USAGE);
    let (_home, source) = source_configured(
        vec![
            row("confirmed", Duration::hours(2)),
            row("onward", Duration::hours(2)),
        ],
        usage_at(&usage_url),
    );
    pin(&source, "confirmed");
    install(provider, &source);
    let (base, requests) = messages_endpoint(vec![
        (429, vec![("retry-after", "120".to_string())], RATE_LIMITED),
        (200, Vec::new(), OK_STREAM),
    ]);
    let served = source.credential().expect("the pinned login").api_key;

    let message = complete(&messages_model(provider, &base), &served);

    assert_eq!(text_of(&message), "hello");
    assert_eq!(
        bearers(&requests),
        vec![
            "sk-ant-oat01-confirmed-store-access-000".to_string(),
            "sk-ant-oat01-onward-store-access-000".to_string()
        ]
    );
    // The request's own due poll, the 429's confirmation (a poll
    // regardless of the reading's age), and the next login's poll before
    // it may take the request (its quota was unknown).
    assert_eq!(usage_hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(
        recorded(&source, "confirmed"),
        Some((Some(30.0), Some(60.0)))
    );
}

#[test]
fn polls_run_on_the_keepalive_thread_never_on_the_request() {
    let provider = "anthropic-quota-poll-background";
    let usage_url = hanging_endpoint();
    let (_home, source) =
        source_configured(vec![row("background", Duration::hours(2))], |config| {
            config.endpoints.usage_url = usage_url.clone();
            config.background = true;
        });
    install(provider, &source);
    let (base, _requests) = messages_endpoint(vec![(200, Vec::new(), OK_STREAM)]);
    let served = source.credential().expect("the login").api_key;
    let model = messages_model(provider, &base);

    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(text_of(&complete(&model, &served)));
    });

    // The poll hangs on the keep-alive thread; the request is answered
    // well inside the poll's 20 s HTTP timeout.
    assert_eq!(
        finished.recv_timeout(std::time::Duration::from_secs(10)),
        Ok("hello".to_string())
    );
}
