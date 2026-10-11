//! The cache keep-alive against the plugin's own runs
//! (`tests/fixtures/golden/pi_extras.json`: the prewarm bodies, the whole
//! prewarm request pi sends, its registry record, `/claude-cachekeep`),
//! ports of the plugin's scheduling tests (`cachekeep.test.ts`,
//! `cachekeep-registry.test.ts`), and the keep-alive end to end over a
//! temporary store and a loopback Messages endpoint.

use std::collections::BTreeMap;

use anthropic::AccountStore;
use anthropic::token::{AccessToken, Credential};
use chrono::{FixedOffset, TimeZone, Utc};
use pa_types::sync::MutexExt;
use serde_json::{Value, json};

use super::prewarm::prewarm_headers;
use super::*;
use crate::shape::{ShapeEnv, ShapeIdentity};
use crate::test_support::*;

fn utc() -> FixedOffset {
    FixedOffset::east_opt(0).expect("UTC")
}

/// `ms` in UTC as the scheduler's local clock.
fn at(ms: i64) -> DateTime<FixedOffset> {
    utc().timestamp_millis_opt(ms).single().expect("a time")
}

/// 2026-05-18T10:00:00Z.
const TRACKED_AT: i64 = 1_779_098_400_000;
const MINUTE: i64 = 60_000;

fn hybrid_always() -> Settings {
    Settings {
        enabled: true,
        always: true,
        window: None,
        hybrid_cache: true,
    }
}

fn body_with_breakpoint() -> String {
    json!({
        "model": "claude-opus-4-7", "max_tokens": 100, "stream": true,
        "system": [{ "type": "text", "text": "stable", "cache_control": { "type": "ephemeral", "ttl": "1h" } }],
        "messages": [{ "role": "user", "content": "hello" }]
    })
    .to_string()
}

fn track(
    keep: &CacheKeep,
    session: &str,
    settings: &Settings,
    now: i64,
) -> Result<(), &'static str> {
    track_body(keep, session, &body_with_breakpoint(), settings, now)
}

fn track_body(
    keep: &CacheKeep,
    session: &str,
    body: &str,
    settings: &Settings,
    now: i64,
) -> Result<(), &'static str> {
    keep.track(
        &Track {
            session_id: Some(session),
            url: "http://127.0.0.1:9/v1/messages",
            headers: &[("authorization".to_string(), "Bearer old".to_string())],
            body_text: body,
            account_id: "login",
        },
        settings,
        &at(now),
    )
}

fn due_ids(tick: &Tick) -> Option<Vec<String>> {
    match tick {
        Tick::Due(targets) => Some(targets.iter().map(|target| target.id.clone()).collect()),
        Tick::Idle | Tick::Inactive => None,
    }
}

// ---------------------------------------------------------------------------
// Golden
// ---------------------------------------------------------------------------

#[test]
fn prewarm_bodies_are_built_as_the_plugin_builds_them() {
    for case in golden_extras()["prewarmBodies"]
        .as_array()
        .expect("prewarm bodies")
    {
        let expected = if case["result"]["ok"] == Value::Bool(true) {
            Ok(case["result"]["bodyText"]
                .as_str()
                .expect("a body")
                .to_string())
        } else {
            Err(case["result"]["reason"].as_str().expect("a reason"))
        };
        assert_eq!(
            prewarm_body(case["bodyText"].as_str().expect("a body")),
            expected,
            "{}",
            case["name"]
        );
    }
}

#[test]
fn a_prewarm_is_the_request_pi_sends() {
    let golden = golden_extras();
    let case = &golden["cachekeep"];
    let original = &case["original"];
    let tracked: Vec<(String, String)> = original["headers"]
        .as_object()
        .expect("headers")
        .iter()
        .map(|(name, value)| (name.clone(), value.as_str().expect("a value").to_string()))
        .collect();
    let identity = ShapeIdentity {
        device_id: case["identity"]["deviceId"].as_str().map(str::to_string),
        account_uuid: case["identity"]["accountUuid"].as_str().map(str::to_string),
        session_id: case["identity"]["sessionId"]
            .as_str()
            .expect("a session")
            .to_string(),
    };

    let body = prewarm_body(original["bodyText"].as_str().expect("the body")).expect("a prewarm");
    let headers: BTreeMap<String, String> = prewarm_headers(
        &tracked,
        case["token"].as_str().expect("the token"),
        &serde_json::from_str(&body).expect("JSON"),
        &identity,
        golden["version"].as_str().expect("the version"),
        &ShapeEnv::default(),
        "request-id",
    )
    .into_iter()
    .filter(|(name, _)| name != "x-client-request-id")
    .collect();

    let prewarm = &case["prewarm"];
    assert_eq!(
        body,
        prewarm["bodyText"].as_str().expect("the prewarm body")
    );
    let expected: BTreeMap<String, String> = prewarm["headers"]
        .as_object()
        .expect("headers")
        .iter()
        .map(|(name, value)| (name.clone(), value.as_str().expect("a value").to_string()))
        .collect();
    assert_eq!(headers, expected);
    // Nothing went out at the 54th minute.
    assert_eq!(case["prewarmsAt54Minutes"], json!(0));
}

#[test]
fn the_registry_record_is_the_plugin_s() {
    let golden = golden_extras();
    let case = &golden["cachekeep"];
    let tracked_at = case["trackedAt"].as_i64().expect("trackedAt");
    let keep = CacheKeep::default();
    track(&keep, "ses-cachekeep", &hybrid_always(), tracked_at).expect("tracked");

    assert_eq!(
        format!("{}\n", record_text(&keep.tracked_sessions(), tracked_at)),
        case["registryRecord"].as_str().expect("the record")
    );
}

#[test]
fn every_cachekeep_command_prints_and_writes_what_pi_does() {
    let mut steps_run = 0;
    for sequence in golden_extras()["commands"].as_array().expect("sequences") {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("anthropic-auth.json");
        if let Some(initial) = sequence["initial"].as_str() {
            std::fs::write(&path, initial).expect("the initial settings");
        }
        let settings = crate::pi::settings::PluginSettings::new(path.clone());
        let registry = Registry::new(directory.path().join("cachekeep"));
        for step in sequence["steps"].as_array().expect("steps") {
            if step["command"] != COMMAND {
                continue;
            }
            steps_run += 1;
            if step["registry"] == Value::Bool(true) {
                // Another live process's record, as the run had it.
                std::fs::create_dir_all(registry.directory()).expect("the registry");
                std::fs::write(
                    registry.directory().join("other-instance.json"),
                    format!(
                        "{}\n",
                        json!({ "version": 1, "updatedAt": Utc::now().timestamp_millis(), "sessions": [
                            { "id": "ses-other-b", "cacheExpiresAt": 1_779_102_000_000_i64, "nextPrewarmAt": 1_779_101_700_000_i64 },
                            { "id": "ses-other-a", "cacheExpiresAt": 1_779_103_000_000_i64, "nextPrewarmAt": 1_779_102_700_000_i64 },
                        ] })
                    ),
                )
                .expect("the other record");
            }
            let args = step["args"].as_str().expect("arguments");
            let text = super::run_command(
                &settings,
                args,
                || registry.list(&[], Utc::now().timestamp_millis()),
                utc(),
            )
            .expect("runs");
            let label = format!("{} /{} {args:?}", sequence["name"], step["command"]);
            assert_eq!(text, step["text"].as_str().expect("the text"), "{label}");
            assert_eq!(
                std::fs::read_to_string(&path).ok(),
                step["file"].as_str().map(str::to_string),
                "{label}"
            );
        }
    }
    assert_eq!(steps_run, 10);
}

// ---------------------------------------------------------------------------
// The plugin's scheduling tests
// ---------------------------------------------------------------------------

#[test]
fn a_tracked_session_is_prewarmed_five_minutes_before_its_cache_expires() {
    let keep = CacheKeep::default();
    let settings = hybrid_always();
    track(&keep, "ses_1", &settings, TRACKED_AT).expect("tracked");

    let at_54 = keep.begin_tick(&settings, &at(TRACKED_AT + 54 * MINUTE));
    let at_55 = keep.begin_tick(&settings, &at(TRACKED_AT + 55 * MINUTE));
    keep.settle("ses_1", &Outcome::Warmed, TRACKED_AT + 55 * MINUTE);

    assert_eq!(
        (due_ids(&at_54), due_ids(&at_55)),
        (Some(vec![]), Some(vec!["ses_1".to_string()]))
    );
    assert_eq!(
        keep.tracked_sessions(),
        vec![TrackedSession {
            id: "ses_1".to_string(),
            cache_expires_at: TRACKED_AT + 115 * MINUTE,
            next_prewarm_at: TRACKED_AT + 110 * MINUTE,
        }]
    );
}

#[test]
fn a_failed_prewarm_backs_off_and_the_session_is_dropped_once_its_cache_expired() {
    let keep = CacheKeep::default();
    let settings = hybrid_always();
    track(&keep, "ses_fail", &settings, TRACKED_AT).expect("tracked");

    let first = keep.begin_tick(&settings, &at(TRACKED_AT + 55 * MINUTE));
    keep.settle(
        "ses_fail",
        &Outcome::Failed(Some(429)),
        TRACKED_AT + 55 * MINUTE,
    );
    // The retry waits out its backoff rather than going every tick.
    let next = keep.begin_tick(&settings, &at(TRACKED_AT + 56 * MINUTE));
    // Once the last success's cache expired, a prewarm would be a paid
    // cold write: the session is dropped.
    let expired = keep.begin_tick(&settings, &at(TRACKED_AT + 60 * MINUTE));

    assert_eq!(
        (due_ids(&first), due_ids(&next), expired),
        (Some(vec!["ses_fail".to_string()]), Some(vec![]), Tick::Idle)
    );
    assert_eq!(keep.tracked_count(), 0);
}

#[test]
fn a_session_that_cannot_be_prewarmed_is_dropped() {
    let keep = CacheKeep::default();
    let settings = hybrid_always();
    track_body(
        &keep,
        "ses_plain",
        "{\"model\":\"m\"}",
        &settings,
        TRACKED_AT,
    )
    .expect("tracked");

    keep.settle(
        "ses_plain",
        &Outcome::Skipped("body has no explicit cache breakpoints"),
        TRACKED_AT + 55 * MINUTE,
    );

    assert_eq!(keep.tracked_count(), 0);
}

#[test]
fn only_hybrid_sessions_inside_the_schedule_are_tracked() {
    let keep = CacheKeep::default();
    let window = Settings {
        enabled: true,
        always: false,
        window: Some(Window {
            start_hour: 9,
            end_hour: 17,
        }),
        hybrid_cache: true,
    };
    // 10:00 UTC is inside 09-17, 18:00 is not.
    assert_eq!(
        [
            track(&keep, "a", &Settings::default(), TRACKED_AT),
            track(
                &keep,
                "b",
                &Settings {
                    hybrid_cache: false,
                    ..window
                },
                TRACKED_AT
            ),
            track(&keep, "c", &window, TRACKED_AT + 8 * 60 * MINUTE),
            track(&keep, "d", &window, TRACKED_AT),
            keep.track(
                &Track {
                    session_id: None,
                    url: "u",
                    headers: &[],
                    body_text: "{}",
                    account_id: "login",
                },
                &window,
                &at(TRACKED_AT),
            ),
        ],
        [
            Err("cachekeep disabled"),
            Err("cache mode is not hybrid"),
            Err("outside configured schedule"),
            Ok(()),
            Err("missing session id"),
        ]
    );
    assert_eq!(keep.tracked_count(), 1);
}

#[test]
fn an_overnight_window_keeps_its_sessions_past_midnight_and_always_keeps_every_day() {
    let overnight = Settings {
        enabled: true,
        always: false,
        window: Some(Window {
            start_hour: 22,
            end_hour: 6,
        }),
        hybrid_cache: true,
    };
    // 23:30 one day, then 00:20 the next: the same window.
    let late = TRACKED_AT + 13 * 60 * MINUTE + 30 * MINUTE;
    let keep = CacheKeep::default();
    track(&keep, "night", &overnight, late).expect("tracked");
    let after_midnight = keep.begin_tick(&overnight, &at(late + 50 * MINUTE));

    let always = hybrid_always();
    let all_day = CacheKeep::default();
    track(&all_day, "day", &always, late).expect("tracked");
    let next_day = all_day.begin_tick(&always, &at(late + 55 * MINUTE));

    assert_eq!(
        (due_ids(&after_midnight), due_ids(&next_day)),
        (Some(vec![]), Some(vec!["day".to_string()]))
    );
    assert_eq!(keep.tracked_count(), 1);
}

#[test]
fn sessions_are_bounded_by_count_and_by_memory() {
    let keep = CacheKeep::default();
    let settings = hybrid_always();
    for index in 0..=MAX_TARGETS {
        track(&keep, &format!("ses-{index:02}"), &settings, TRACKED_AT).expect("tracked");
    }
    let oldest_kept = keep
        .tracked_sessions()
        .first()
        .map(|session| session.id.clone());
    let big = format!("{{\"x\":\"{}\"}}", "a".repeat(MAX_BODY_UNITS / 2));
    let memory = CacheKeep::default();
    track_body(&memory, "first", &big, &settings, TRACKED_AT).expect("tracked");
    track_body(&memory, "second", &big, &settings, TRACKED_AT).expect("tracked");
    let too_big = format!("{{\"x\":\"{}\"}}", "a".repeat(MAX_BODY_UNITS));

    assert_eq!(
        (keep.tracked_count(), oldest_kept.as_deref()),
        (MAX_TARGETS, Some("ses-01"))
    );
    assert_eq!(
        memory
            .tracked_sessions()
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>(),
        vec!["second".to_string()]
    );
    assert_eq!(
        track_body(&memory, "huge", &too_big, &settings, TRACKED_AT),
        Err("body exceeds cachekeep memory budget")
    );
}

#[test]
fn retry_delays_are_the_plugin_s() {
    // Computed with cachekeep.ts's `cacheKeepRetryDelayMs` (7f5d88a).
    assert_eq!(
        [
            retry_delay_ms("ses_fail", 1),
            retry_delay_ms("ses_fail", 2),
            retry_delay_ms("x", 9),
            retry_delay_ms("ünï😀", 3),
            retry_delay_ms("session-1", 0),
            retry_delay_ms("session-1", 4),
        ],
        [64_235, 123_738, 901_782, 246_660, 73_641, 488_859]
    );
}

#[test]
fn the_registry_lists_live_records_with_the_newest_expiry() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let now = Utc::now().timestamp_millis();
    let session = |id: &str, expires: i64| TrackedSession {
        id: id.to_string(),
        cache_expires_at: expires,
        next_prewarm_at: expires - PREWARM_LEAD_MS,
    };
    let one = Registry::new(directory.path().to_path_buf());
    let two = Registry::new(directory.path().to_path_buf());
    one.publish(&[session("b", now + 10), session("shared", now + 5)], now);
    two.publish(&[session("shared", now + 50)], now);
    std::fs::write(
        directory.path().join("dead.json"),
        format!(
            "{}\n",
            record_text(&[session("dead", now + 99)], now - 4 * MINUTE)
        ),
    )
    .expect("an expired lease");
    std::fs::write(directory.path().join("junk.json"), "{nope").expect("junk");

    let listed = one.list(&[session("a", now + 1)], now);
    two.publish(&[], now);
    let after = one.list(&[], now);

    assert_eq!(
        listed,
        vec![
            session("a", now + 1),
            session("b", now + 10),
            session("shared", now + 50)
        ]
    );
    assert_eq!(
        after,
        vec![session("b", now + 10), session("shared", now + 5)]
    );
}

#[test]
fn cachekeep_arguments_parse_as_the_plugin_parses_them() {
    assert_eq!(
        [
            "",
            " off ",
            "always",
            "9-17",
            "09-5",
            "7-7",
            "24-1",
            "1-2-3",
            "subagents on",
            "x"
        ]
        .map(parse_command),
        [
            Action::Status,
            Action::Disable,
            Action::Always,
            Action::Window(Window {
                start_hour: 9,
                end_hour: 17
            }),
            Action::Window(Window {
                start_hour: 9,
                end_hour: 5
            }),
            Action::Usage,
            Action::Usage,
            Action::Usage,
            Action::Subagents(true),
            Action::Usage,
        ]
    );
}

// ---------------------------------------------------------------------------
// End to end
// ---------------------------------------------------------------------------

#[test]
fn a_kept_session_is_prewarmed_with_the_store_s_current_token() {
    let provider = "anthropic-cachekeep";
    let (_home, source) = source_over(
        vec![row_with_account(
            "cachekeep-login",
            Some("00000000-0000-4000-8000-000000000001"),
        )],
        "http://127.0.0.1:9",
    );
    write_device_id(&source, &"a".repeat(64));
    write_pi_settings(
        &source,
        &json!({ "claudeCache": { "enabled": true, "mode": "hybrid" }, "cacheKeep": { "enabled": true, "always": true }, "accounts": [] }),
    );
    pa_core::auth::install_credential_source(provider, source.clone());
    pa_ai::request_hooks::install_request_hooks(provider, source.clone());
    let (base, requests) = messages_endpoint(vec![
        (200, Vec::new(), OK_STREAM),
        (
            200,
            Vec::new(),
            r#"{"usage":{"cache_read_input_tokens":12}}"#,
        ),
    ]);
    let model = model_with_id(provider, &base, "claude-opus-4-8");
    let served = pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
        .expect("the store's token")
        .api_key;

    complete(&model, &served);
    let tracked = source.cachekeep.tracked_sessions();
    let registry_files =
        std::fs::read_dir(&source.config.cachekeep_registry_dir).map_or(0, Iterator::count);
    // Another process rotates the login's token before the prewarm.
    AccountStore::mutate(source.store_path(), |store| {
        let account = store.get_mut("cachekeep-login")?;
        if let Credential::Oauth(tokens) = &mut account.credential {
            tokens.access = AccessToken::new("sk-ant-oat01-cachekeep-rotated-000".to_string());
        }
        Ok(())
    })
    .expect("rotate the token");
    let first = tracked.first().map_or(0, |session| session.next_prewarm_at);
    let parked = source.cachekeep_tick(&at(first));

    assert_eq!(
        tracked
            .iter()
            .map(|session| session.id.as_str())
            .collect::<Vec<_>>(),
        vec!["session-1"]
    );
    assert_eq!((registry_files, parked), (1, false));
    let requests = requests.lock_or_recover().clone();
    assert_eq!(requests.len(), 2);
    let prewarm: Value = serde_json::from_str(&requests[1].body).expect("a JSON body");
    let original: Value = serde_json::from_str(&requests[0].body).expect("a JSON body");
    let mut expected = original;
    expected["max_tokens"] = json!(0);
    expected
        .as_object_mut()
        .expect("an object")
        .shift_remove("stream");
    assert_eq!(
        (requests[1].bearer(), prewarm),
        ("sk-ant-oat01-cachekeep-rotated-000".to_string(), expected)
    );
    assert!(
        requests[1]
            .header("anthropic-beta")
            .is_some_and(|betas| betas.split(',').any(|beta| beta == EXTENDED_TTL_BETA))
    );
    // The session's cache was renewed: an hour from the prewarm, renewed
    // again five minutes before.
    assert_eq!(
        source
            .cachekeep
            .tracked_sessions()
            .first()
            .map(|session| (session.cache_expires_at, session.next_prewarm_at)),
        Some((first + TTL_MS, first + TTL_MS - PREWARM_LEAD_MS))
    );
}

#[test]
fn a_tracked_request_starts_the_scheduler_on_its_own_thread() {
    let provider = "anthropic-cachekeep-thread";
    let (_home, source) =
        source_configured(vec![row_with_account("cachekeep-thread", None)], |config| {
            config.background = true;
        });
    write_pi_settings(
        &source,
        &json!({ "claudeCache": { "enabled": true, "mode": "hybrid" }, "cacheKeep": { "enabled": true, "always": true }, "accounts": [] }),
    );
    pa_core::auth::install_credential_source(provider, source.clone());
    pa_ai::request_hooks::install_request_hooks(provider, source.clone());
    let (base, _requests) = messages_endpoint(vec![(200, Vec::new(), OK_STREAM)]);
    let model = model_with_id(provider, &base, "claude-opus-4-8");
    let served = pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
        .expect("the store's token")
        .api_key;

    complete(&model, &served);

    // The registry is written by the scheduler's thread.
    let registry = source.config.cachekeep_registry_dir.clone();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::fs::read_dir(&registry).map_or(0, Iterator::count) == 0 {
        assert!(std::time::Instant::now() < deadline, "no registry record");
        std::thread::yield_now();
    }
    assert!(source.cachekeep_jobs.get().is_some());
}
