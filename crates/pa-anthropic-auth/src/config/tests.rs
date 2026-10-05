//! The sidecar's path, schema and defaults, against temporary files only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anthropic::killswitch::{
    KillswitchConfig, KillswitchThresholds, DEFAULT_KILLSWITCH_THRESHOLDS,
};
use anthropic::quota::QuotaPolicy;
use anthropic::sticky_routing::RoutingMode;
use serde_json::json;

use super::*;

fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    move |key| {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    }
}

#[test]
fn the_sidecar_resolves_to_the_plugin_copy_the_user_has() {
    let home = Path::new("/home/someone");
    let pi = PathBuf::from("/home/someone/.pi/agent/anthropic-auth.json");
    let opencode = PathBuf::from("/home/someone/.config/opencode/anthropic-auth.json");
    let none = |_: &Path| false;
    let only_pi = |path: &Path| path == Path::new("/home/someone/.pi/agent/anthropic-auth.json");
    let both = |_: &Path| true;
    assert_eq!(
        [
            // Nothing exists: opencode's path, where a first write creates it.
            resolve_config_path(lookup(&[]), home, none),
            // Only opencode's exists (the common case): opencode's.
            resolve_config_path(lookup(&[]), home, |path: &Path| path == opencode),
            // pi's exists: pi's wins over opencode's.
            resolve_config_path(lookup(&[]), home, only_pi),
            resolve_config_path(lookup(&[]), home, both),
            // Directory overrides move each candidate.
            resolve_config_path(lookup(&[(AGENT_DIR_ENV, " /agents/pi ")]), home, both),
            resolve_config_path(lookup(&[(OPENCODE_CONFIG_DIR_ENV, "/oc")]), home, none),
            resolve_config_path(lookup(&[("XDG_CONFIG_HOME", "/xdg")]), home, none),
            // Explicit file overrides win, pi's first; blank ones are ignored.
            resolve_config_path(
                lookup(&[
                    (CONFIG_FILE_ENV, " /etc/auth.json "),
                    (OPENCODE_CONFIG_FILE_ENV, "/etc/oc.json"),
                    (AGENT_DIR_ENV, "/agents/pi")
                ]),
                home,
                both
            ),
            resolve_config_path(
                lookup(&[(OPENCODE_CONFIG_FILE_ENV, "/etc/oc.json")]),
                home,
                both
            ),
            resolve_config_path(lookup(&[(CONFIG_FILE_ENV, "  ")]), home, none),
        ],
        [
            opencode.clone(),
            opencode.clone(),
            pi.clone(),
            pi,
            PathBuf::from("/agents/pi/anthropic-auth.json"),
            PathBuf::from("/oc/anthropic-auth.json"),
            PathBuf::from("/xdg/opencode/anthropic-auth.json"),
            PathBuf::from("/etc/auth.json"),
            PathBuf::from("/etc/oc.json"),
            opencode,
        ]
    );
}

#[test]
fn the_routing_state_lives_beside_the_sidecar() {
    let config = Path::new("/home/someone/.pi/agent/anthropic-auth.json");
    assert_eq!(
        [
            routing_state_path_from_lookup(lookup(&[]), config),
            routing_state_path_from_lookup(
                lookup(&[(ROUTING_STATE_ENV, "/tmp/state.json")]),
                config
            ),
        ],
        [
            PathBuf::from("/home/someone/.pi/agent/anthropic-auth-routing-state.json"),
            PathBuf::from("/tmp/state.json"),
        ]
    );
}

#[test]
fn every_setting_the_routing_reads_is_parsed() {
    let document = json!({
        "accounts": [],
        "routing": { "mode": "sticky-balanced" },
        "quota": {
            "enabled": true,
            "checkIntervalMinutes": 2,
            "refreshEveryNRequests": 7.9,
            "minimumRemaining": { "5h": 12, "seven_day": 20 },
            "failClosedOnUnknownQuota": false
        },
        "killswitch": {
            "enabled": true,
            "main": { "five_hour": 3, "1w": 8 },
            "accounts": { "work": { "5h": 5, "seven_day": 10, "scoped": 1 } }
        }
    });

    assert_eq!(
        parse_config(&document),
        RoutingConfig {
            mode: RoutingMode::StickyBalanced,
            quota: QuotaPolicy {
                enabled: true,
                check_interval_ms: 120_000,
                minimum_remaining_five_hour: 12.0,
                minimum_remaining_seven_day: 20.0,
                fail_closed_on_unknown: false,
                refresh_every_n_requests: 7,
            },
            killswitch: KillswitchConfig {
                enabled: true,
                main: Some(KillswitchThresholds {
                    five_hour: Some(3.0),
                    seven_day_alias: Some(8.0),
                    ..KillswitchThresholds::default()
                }),
                accounts: BTreeMap::from([(
                    "work".to_string(),
                    KillswitchThresholds {
                        five_hour_alias: Some(5.0),
                        seven_day: Some(10.0),
                        scoped: Some(1.0),
                        ..KillswitchThresholds::default()
                    }
                )]),
            },
        }
    );
}

#[test]
fn an_empty_or_foreign_document_is_the_plugins_defaults() {
    let defaults = RoutingConfig {
        mode: RoutingMode::MainFirst,
        quota: QuotaPolicy {
            enabled: true,
            check_interval_ms: 300_000,
            minimum_remaining_five_hour: 0.0,
            minimum_remaining_seven_day: 0.0,
            fail_closed_on_unknown: true,
            refresh_every_n_requests: 0,
        },
        killswitch: KillswitchConfig::default(),
    };
    for document in [
        json!({}),
        json!([1, 2]),
        json!("text"),
        json!({ "accounts": [] }),
    ] {
        assert_eq!(parse_config(&document), defaults, "{document}");
    }
}

#[test]
fn values_of_the_wrong_type_are_their_defaults() {
    let document = json!({
        "routing": { "mode": "Sticky-Balanced" },
        "quota": {
            "enabled": "no",
            "checkIntervalMinutes": 0.25,
            "refreshEveryNRequests": -3,
            "minimumRemaining": { "five_hour": "7", "5h": 40 },
            "failClosedOnUnknownQuota": "yes"
        },
        "killswitch": {
            "enabled": "true",
            "main": { "five_hour": "1", "5h": 2 }
        }
    });

    let config = parse_config(&document);

    assert_eq!(
        (config.mode, config.quota),
        (
            RoutingMode::MainFirst,
            QuotaPolicy {
                enabled: true,
                // Floored at a minute, as `Math.max(1, minutes)`.
                check_interval_ms: 60_000,
                minimum_remaining_five_hour: 0.0,
                minimum_remaining_seven_day: 0.0,
                fail_closed_on_unknown: true,
                refresh_every_n_requests: 0,
            }
        )
    );
    assert!(!config.killswitch.enabled);
    // `five_hour: "1"` shadows the alias and resolves to the default.
    assert_eq!(
        config.killswitch.thresholds_for(None),
        anthropic::killswitch::ResolvedThresholds {
            five_hour: DEFAULT_KILLSWITCH_THRESHOLDS.five_hour,
            ..DEFAULT_KILLSWITCH_THRESHOLDS
        }
    );
}

#[test]
fn the_file_is_re_read_when_it_changes() {
    let dir = tempfile::tempdir().expect("a temporary dir");
    let path = dir.path().join("anthropic-auth.json");
    let file = ConfigFile::new(Some(path.clone()));

    let missing = file.current();
    std::fs::write(
        &path,
        r#"{"accounts":[],"routing":{"mode":"fallback-first"}}"#,
    )
    .expect("write the sidecar");
    let written = file.current();
    std::fs::write(&path, "{ not json").expect("corrupt the sidecar");
    let corrupt = file.current();

    assert_eq!(
        [missing.mode, written.mode, corrupt.mode],
        [
            RoutingMode::MainFirst,
            RoutingMode::FallbackFirst,
            RoutingMode::MainFirst
        ]
    );
    assert_eq!(*ConfigFile::new(None).current(), RoutingConfig::default());
}
