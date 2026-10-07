//! Ported from the skill's `tests/test_policy.py` (gate outcomes, settings
//! loading, the allowlist summary) and `test_w1_security.py`'s built-in
//! deny invariant.

use super::*;
use crate::testing::write_settings;

const ALLOWED: &str = "com.example.app";
const OTHER: &str = "com.other.app";
const BLOCKED: &str = "com.blocked.app";
const CUSTOM_DENY: &str = "com.custom.deny";

fn path() -> PathBuf {
    PathBuf::from("/agent/settings/computer-use.toml")
}

fn settings(allowed: &[&str], blocked: &[&str], system_deny: &[&str]) -> Settings {
    let mut settings = Settings {
        allowed: allowed.iter().map(ToString::to_string).collect(),
        blocked: blocked.iter().map(ToString::to_string).collect(),
        ..Settings::default()
    };
    settings
        .system_deny
        .extend(system_deny.iter().map(ToString::to_string));
    settings
}

fn allowed(verdict: &GateVerdict) -> bool {
    matches!(verdict, GateVerdict::Allowed { .. })
}

fn reason(verdict: &GateVerdict) -> &str {
    match verdict {
        GateVerdict::Allowed { .. } => "",
        GateVerdict::Denied { reason, .. } => reason,
    }
}

#[test]
fn an_allowed_app_passes() {
    let verdict = gate(ALLOWED, &settings(&[ALLOWED], &[], &[]), &path());
    assert_eq!(verdict, GateVerdict::Allowed { risk: Risk::Medium });
}

#[test]
fn an_app_off_the_allowlist_is_denied_with_an_actionable_reason() {
    let verdict = gate(OTHER, &settings(&[ALLOWED], &[], &[]), &path());
    assert_eq!(
        verdict,
        GateVerdict::Denied {
            denial: Denial::NotAllowlisted,
            reason: "com.other.app is not on the allowlist. To allow it, add its bundle id to \
                     `apps.allowed` in /agent/settings/computer-use.toml:\n\n    [apps]\n    \
                     allowed = [\"com.other.app\"]\n\nThe allowlist is user-edited; Prime Agent \
                     never edits it."
                .to_string(),
            risk: Risk::Medium,
        }
    );
}

#[test]
fn a_blocked_app_is_denied_naming_the_bundle_and_the_settings_path() {
    let verdict = gate(BLOCKED, &settings(&[BLOCKED], &[BLOCKED], &[]), &path());
    assert_eq!(
        verdict,
        GateVerdict::Denied {
            denial: Denial::Blocked,
            reason: "com.blocked.app is on the blocked list; remove it from `apps.blocked` in \
                     /agent/settings/computer-use.toml to use it. To allow an app, add its bundle \
                     id to `apps.allowed` in the same file."
                .to_string(),
            risk: Risk::Medium,
        }
    );
}

#[test]
fn the_builtin_system_deny_holds_on_default_settings_and_over_the_allowlist() {
    for bundle in SYSTEM_DENY {
        assert!(!allowed(&gate(bundle, &Settings::default(), &path())));
        let verdict = gate(bundle, &settings(&SYSTEM_DENY, &[], &[]), &path());
        assert!(reason(&verdict).contains("system deny-list"), "{verdict:?}");
    }
}

#[test]
fn a_settings_value_cannot_allow_a_builtin_system_deny_entry() {
    // test_w1_security BuiltinDenyInvariantTests: even a Settings whose
    // system_deny list lost the built-ins still refuses them.
    let settings = Settings {
        allowed: vec!["com.apple.loginwindow".to_string()],
        system_deny: vec![CUSTOM_DENY.to_string()],
        ..Settings::default()
    };
    let verdict = gate("com.apple.loginwindow", &settings, &path());
    assert!(matches!(
        verdict,
        GateVerdict::Denied {
            denial: Denial::SystemDeny,
            ..
        }
    ));
}

#[test]
fn the_files_system_deny_unions_the_builtin_list() {
    let settings = settings(&[ALLOWED], &[], &[CUSTOM_DENY]);
    assert!(!allowed(&gate(CUSTOM_DENY, &settings, &path())));
    for bundle in SYSTEM_DENY {
        assert!(!allowed(&gate(bundle, &settings, &path())));
    }
}

#[test]
fn the_verdict_carries_the_risk_label_defaulting_to_medium() {
    let mut labelled = settings(&[ALLOWED], &[], &[]);
    labelled.risk.push((ALLOWED.to_string(), Risk::High));
    assert_eq!(
        gate(ALLOWED, &labelled, &path()),
        GateVerdict::Allowed { risk: Risk::High }
    );
    assert_eq!(
        gate(ALLOWED, &settings(&[ALLOWED], &[], &[]), &path()),
        GateVerdict::Allowed { risk: Risk::Medium }
    );
}

#[test]
fn a_missing_file_loads_the_tolerant_defaults() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        Settings::load(&dir.path().join("missing.toml")),
        Settings::default()
    );
}

#[test]
fn the_fixture_file_loads_every_section() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_settings(
        dir.path(),
        &[ALLOWED],
        &[BLOCKED],
        &[CUSTOM_DENY],
        &[(ALLOWED, "low")],
    );
    let loaded = Settings::load(&file);
    assert_eq!(
        loaded,
        Settings {
            allowed: vec![ALLOWED.to_string()],
            blocked: vec![BLOCKED.to_string()],
            system_deny: vec![
                SYSTEM_DENY[0].to_string(),
                SYSTEM_DENY[1].to_string(),
                CUSTOM_DENY.to_string()
            ],
            risk: vec![(ALLOWED.to_string(), Risk::Low)],
        }
    );
}

#[test]
fn unparsable_or_non_utf8_files_load_the_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "not [valid {{{ toml").unwrap();
    assert_eq!(Settings::load(&bad), Settings::default());
    std::fs::write(&bad, b"[apps]\nallowed = [\"\xff\"]\n").unwrap();
    assert_eq!(Settings::load(&bad), Settings::default());
}

#[test]
fn junk_documents_never_fail() {
    let document: toml::Table = "apps = 42\nsystem_deny = \"no\"\n[risk]\nx = 1\n"
        .parse()
        .unwrap();
    assert_eq!(Settings::from_document(&document), Settings::default());
}

#[test]
fn only_valid_risk_labels_are_kept_in_file_order() {
    let document: toml::Table =
        "[risk]\n\"com.b\" = \"high\"\n\"com.a\" = \"low\"\n\"com.c\" = \"severe\"\n"
            .parse()
            .unwrap();
    assert_eq!(
        Settings::from_document(&document).risk,
        vec![
            ("com.b".to_string(), Risk::High),
            ("com.a".to_string(), Risk::Low)
        ]
    );
}

#[test]
fn duplicate_and_non_string_list_entries_are_dropped() {
    let document: toml::Table = "[apps]\nallowed = [\"a\", 3, \"b\", \"a\"]\n"
        .parse()
        .unwrap();
    assert_eq!(Settings::from_document(&document).allowed, ["a", "b"]);
}

#[test]
fn the_summary_reads_the_file_at_call_time() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path();
    std::fs::create_dir_all(agent_dir.join("settings")).unwrap();
    let policy = Policy::for_agent_dir(agent_dir);
    write_settings(
        &agent_dir.join("settings"),
        &[ALLOWED],
        &[BLOCKED],
        &[CUSTOM_DENY],
        &[(ALLOWED, "high")],
    );
    assert_eq!(
        policy.summary(),
        json!({
            "allowed": [ALLOWED],
            "blocked": [BLOCKED],
            "system_deny": [SYSTEM_DENY[0], SYSTEM_DENY[1], CUSTOM_DENY],
            "risk": {ALLOWED: "high"},
        })
    );
    write_settings(
        &agent_dir.join("settings"),
        &[ALLOWED, OTHER],
        &[],
        &[],
        &[],
    );
    assert_eq!(policy.summary()["allowed"], json!([ALLOWED, OTHER]));
    assert!(allowed(&policy.gate(OTHER)));
    assert!(!allowed(&policy.gate(BLOCKED)));
}
