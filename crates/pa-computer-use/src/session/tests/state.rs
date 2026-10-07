//! `get_state`, `list_apps`, `permissions_status` (`test_api`'s
//! `ModuleGetStateTests`).

use serde_json::json;

use super::*;
use crate::error::transport;
use crate::permissions::{PermissionState, MAC_HELP_LINES};
use crate::policy::SYSTEM_DENY;
use crate::telemetry::{Outcome, TelemetryEvent};

#[test]
fn get_state_reports_apps_permissions_allowlist_and_platform() {
    let env = Env::new();
    assert_eq!(
        env.session.get_state(true).unwrap(),
        json!({
            "apps": [{"id": BUNDLE, "name": "Example", "running": true}],
            "permissions": {"accessibility": "ok", "screen_recording": "ok", "help": MAC_HELP_LINES},
            "allowlist": {"allowed": [BUNDLE], "blocked": [], "system_deny": SYSTEM_DENY, "risk": {}},
            "platform": "mac",
        })
    );
}

#[test]
fn get_state_emits_the_session_start_once_then_a_get_state_action_each_time() {
    let env = Env::new();
    env.session.get_state(true).unwrap();
    env.session.get_state(true).unwrap();
    let events = env.telemetry.events();
    let started: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, TelemetryEvent::SessionStarted { .. }))
        .collect();
    assert_eq!(
        started,
        [&TelemetryEvent::SessionStarted { platform: "mac" }]
    );
    assert_eq!(
        env.actions(),
        [("get_state", Outcome::Ok), ("get_state", Outcome::Ok)]
    );
}

#[test]
fn get_state_with_emit_false_is_silent() {
    let env = Env::new();
    env.session.get_state(false).unwrap();
    assert!(env.telemetry.events().is_empty());
}

#[test]
fn get_state_reads_a_transport_failure_as_no_apps() {
    let env = Env::new();
    env.fake().running_error = Some(transport("no workspace"));
    let state = env.session.get_state(true).unwrap();
    assert_eq!(state["apps"], json!([]));
    assert_eq!(state["platform"], json!("mac"));
}

#[test]
fn get_state_propagates_other_listing_failures() {
    let env = Env::new();
    env.fake().running_error = Some(ComputerUseError::new(ErrorCode::InvalidArgument, "bad"));
    assert_eq!(
        code(env.session.get_state(true)),
        ErrorCode::InvalidArgument
    );
}

#[test]
fn list_apps_and_permissions_have_the_contract_shape() {
    let env = Env::new();
    assert_eq!(
        super::super::apps_json(&env.session.list_apps().unwrap()),
        json!([{"id": BUNDLE, "name": "Example", "running": true}])
    );
    env.fake().screen_recording = PermissionState::Missing;
    assert_eq!(
        env.session.permissions(),
        json!({"accessibility": "ok", "screen_recording": "missing", "help": MAC_HELP_LINES})
    );
}
