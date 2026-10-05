//! The account commands against the plugins' own runs
//! (`tests/fixtures/golden/pi_extras.json`): the text each prints and the
//! settings file after it, byte for byte; the quota summary's text; and the
//! commands through the session feature over a temporary store.

use std::sync::Arc;

use anthropic::AccountStore;
use chrono::Duration;
use serde_json::{json, Value};

use super::*;
use crate::test_support::*;

/// The login ids the golden's `/claude-killswitch` runs listed.
fn golden_logins() -> Vec<String> {
    vec!["acct-a".to_string(), "acct-b".to_string()]
}

#[test]
fn every_account_command_prints_and_writes_what_the_plugins_do() {
    let mut steps_run = 0;
    for sequence in golden_extras()["commands"].as_array().expect("sequences") {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("anthropic-auth.json");
        if let Some(initial) = sequence["initial"].as_str() {
            std::fs::write(&path, initial).expect("the initial settings");
        }
        let settings = PluginSettings::new(path.clone());
        for step in sequence["steps"].as_array().expect("steps") {
            let args = step["args"].as_str().expect("arguments");
            let text = match step["command"].as_str().expect("a command") {
                ROUTING_COMMAND => run_routing(&settings, args, || Ok(())).expect("runs"),
                KILLSWITCH_COMMAND => {
                    run_killswitch(&settings, args, &golden_logins()).expect("runs")
                }
                _ => continue,
            };
            steps_run += 1;
            let label = format!("{} /{} {args:?}", sequence["name"], step["command"]);
            assert_eq!(text, step["text"].as_str().expect("the text"), "{label}");
            assert_eq!(
                std::fs::read_to_string(&path).ok(),
                step["file"].as_str().map(str::to_string),
                "{label}"
            );
        }
    }
    assert_eq!(steps_run, 19);
}

/// A golden quota case's accounts as the summary takes them.
fn golden_accounts(case: &Value) -> Vec<QuotaAccountSummary> {
    case["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .map(|account| QuotaAccountSummary {
            name: account["name"].as_str().expect("a name").to_string(),
            main: account["role"] == "main",
            enabled: account["enabled"].as_bool(),
            quota: (!account["quota"].is_null()).then(|| {
                serde_json::from_value(account["quota"].clone()).expect("a quota snapshot")
            }),
            last_refreshed_at: account["lastRefreshedAt"].as_i64(),
            error: account["error"].as_str().map(str::to_string),
            tier_label: account["tierLabel"].as_str().map(str::to_string),
        })
        .collect()
}

#[test]
fn the_quota_summary_reads_as_the_plugins() {
    for case in golden_extras()["quota"].as_array().expect("quota cases") {
        assert_eq!(
            quota_text(&golden_accounts(case), case["now"].as_i64().expect("now")),
            case["text"].as_str().expect("the text"),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn routing_modes_parse_as_the_plugins_parse_them() {
    assert_eq!(
        [
            "",
            "Sticky-Balanced",
            "mode fallback-first",
            "mode",
            "reset",
            "reset now",
            "fallback-first extra"
        ]
        .map(parse_routing),
        [
            RoutingAction::Status,
            RoutingAction::Mode("sticky-balanced"),
            RoutingAction::Mode("fallback-first"),
            RoutingAction::Usage,
            RoutingAction::Reset,
            RoutingAction::Usage,
            RoutingAction::Usage,
        ]
    );
}

#[test]
fn killswitch_entries_parse_as_the_plugins_parse_them() {
    assert_eq!(
        parse_killswitch(" set main:3,8,0  work:07,10 "),
        KillswitchAction::Set(vec![
            KillswitchEntry {
                account: "main".to_string(),
                five_hour: 3.0,
                seven_day: 8.0,
                scoped: Some(0.0),
            },
            KillswitchEntry {
                account: "work".to_string(),
                five_hour: 7.0,
                seven_day: 10.0,
                scoped: None,
            },
        ])
    );
    for usage in [
        "set",
        "set a:1",
        "set :1,2",
        "set a:1,2,3,4",
        "set a:1,x",
        "on off",
    ] {
        assert_eq!(parse_killswitch(usage), KillswitchAction::Usage, "{usage}");
    }
}

/// A source over `ids` (each labelled `<id> label` but the last), its
/// sidecar the pi settings file, and the sticky state beside it.
fn commands_source(ids: &[&str]) -> (tempfile::TempDir, Arc<crate::SharedStoreSource>) {
    let mut rows: Vec<anthropic::Account> =
        ids.iter().map(|id| row(id, Duration::hours(2))).collect();
    for account in rows.iter_mut().take(ids.len().saturating_sub(1)) {
        account.label = Some(format!("{} label", account.id));
    }
    source_configured(rows, |config| {
        config.routing_state_path = Some(
            config
                .pi
                .settings_path
                .with_file_name("anthropic-auth-routing-state.json"),
        );
    })
}

#[test]
fn the_killswitch_lists_the_store_s_logins_and_the_routing_reads_what_it_writes() {
    let (_home, source) = commands_source(&["ks-one", "ks-two"]);
    let feature = crate::AnthropicAuthFeature::new(source.clone());

    let text = run_command(&feature, KILLSWITCH_COMMAND, "set ks-two:40,50");

    assert_eq!(
        text,
        "## Killswitch Updated\n\n## Killswitch\n\nStatus: **ON**\n\n\
         | Account | 5h threshold | 1w threshold | Scoped |\n\
         | ------- | ------------ | ------------ | ------ |\n\
         | main | \u{2265} 5% | \u{2265} 10% | \u{2264} 0% |\n\
         | ks-one | \u{2265} 5% | \u{2265} 10% | \u{2264} 0% |\n\
         | ks-two | \u{2265} 40% | \u{2265} 50% | \u{2264} 0% |"
    );
    let killswitch = source.settings().killswitch.clone();
    assert_eq!(
        (
            killswitch.enabled,
            killswitch
                .accounts
                .get("ks-two")
                .map(|thresholds| (thresholds.five_hour, thresholds.seven_day))
        ),
        (true, Some((Some(40.0), Some(50.0))))
    );
}

#[test]
fn routing_reset_clears_the_session_s_sticky_assignment() {
    use sha2::Digest;
    let (_home, source) = commands_source(&["reset-one"]);
    let state_path = source
        .config
        .routing_state_path
        .clone()
        .expect("a state path");
    let now = chrono::Utc::now().timestamp_millis();
    let key = |session: &str| {
        sha2::Sha256::digest(session.as_bytes())
            .iter()
            .fold(String::new(), |mut key, byte| {
                use std::fmt::Write;
                let _ = write!(key, "{byte:02x}");
                key
            })
    };
    let assignment = json!({
        "accountId": "reset-one", "family": "opus", "assignedAt": now, "lastSeenAt": now,
        "initialInputBytes": 10, "quotaCheckedAt": now
    });
    std::fs::write(
        &state_path,
        json!({ "version": 1, "updatedAt": now, "assignments": {
            key("pi-commands"): assignment, key("another-session"): assignment
        } })
        .to_string(),
    )
    .expect("seed the sticky state");
    let feature = crate::AnthropicAuthFeature::new(source);

    let text = run_command(&feature, ROUTING_COMMAND, "reset");

    assert!(
        text.starts_with("## Claude Routing Assignment Reset\n"),
        "{text}"
    );
    let state: Value =
        serde_json::from_slice(&std::fs::read(&state_path).expect("the state")).expect("JSON");
    let kept: Vec<&String> = state["assignments"]
        .as_object()
        .expect("assignments")
        .keys()
        .collect();
    assert_eq!(kept, vec![&key("another-session")]);
}

#[test]
fn the_quota_summary_lists_the_store_s_logins_with_their_readings() {
    let (_home, source) = commands_source(&["quota-main", "quota-spare"]);
    AccountStore::mutate(source.store_path(), |store| {
        store.set_current("quota-main")?;
        let spare = store.get_mut("quota-spare")?;
        spare.enabled = false;
        Ok(())
    })
    .expect("pin the main login");
    let feature = crate::AnthropicAuthFeature::new(source);

    let text = run_command(&feature, QUOTA_COMMAND, "");

    assert_eq!(
        text,
        "## Claude Quotas\n\n\
         ### quota-main label (main)\n  - 5h: unknown\n  - 1w: unknown\n\n\
         ### quota-spare (fallback disabled)\n  - 5h: unknown\n  - 1w: unknown"
    );
}

#[test]
fn money_reads_as_intl_writes_it() {
    let money = |amount_minor: f64, currency: &str, exponent: u32| {
        super::money(&QuotaMoney {
            amount_minor,
            currency: currency.to_string(),
            exponent,
        })
    };
    assert_eq!(
        [
            money(123_456.0, "USD", 2),
            money(-5.0, "usd", 2),
            money(150_000.0, "JPY", 0),
            money(1.0, "XQZ", 2),
            money(7.0, "US", 2),
        ],
        [
            "$1,234.56".to_string(),
            "-$0.05".to_string(),
            "¥150,000".to_string(),
            "XQZ\u{a0}0.01".to_string(),
            "7 US".to_string(),
        ]
    );
}
