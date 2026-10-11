//! `get_app` on the workspace model: `test_api`'s `GetAppTests`, `test_w1`'s
//! dotted-name, launch-gate, fail-closed-launch and guarded-bind cases.

use serde_json::json;

use super::*;
use crate::permissions::PermissionState;
use crate::platform::RunningApp;
use crate::session::BoundApp;
use crate::session::fake::{NAME, PID};

fn running(bundle_id: &str, name: &str, pid: i64) -> RunningApp {
    RunningApp {
        bundle_id: bundle_id.to_string(),
        name: name.to_string(),
        pid,
        path: None,
    }
}

#[test]
fn get_app_binds_and_loads_the_first_full_state() {
    let env = Env::new();
    let app = bound(&env);
    assert_eq!(
        (app.bundle_id.as_str(), app.name.as_str(), app.pid),
        (BUNDLE, NAME, PID)
    );
    let state = app.state.unwrap();
    assert!(state.starts_with(
        "Example (com.example.app) — window 'Main' — 5 elements, indices [0]..[4]\n[0] AXStaticText"
    ), "{state}");
    assert!(state.contains("'Search'") && state.contains("'Save'"));
    // The bind loads state but is not an action: no telemetry.
    assert!(env.actions().is_empty());
}

#[test]
fn a_second_bind_of_the_same_pid_reuses_the_binding() {
    let env = Env::new();
    let first = bound(&env);
    let again = bound(&env);
    assert_eq!(again, first);
    env.fake().running = vec![running(BUNDLE, NAME, 9999)];
    env.fake().launch_result = None;
    assert_ne!(bound(&env).handle, first.handle);
}

#[test]
fn an_unallowed_app_is_denied_naming_the_settings_file() {
    let env = Env::allowing(&["com.other.app"]);
    let denied = bind(&env, BUNDLE).unwrap_err();
    assert_eq!(denied.code, ErrorCode::AppNotAllowed);
    assert!(denied.message.contains(BUNDLE));
    let settings = env
        .agent_dir
        .path()
        .join("settings")
        .join("computer-use.toml");
    assert!(denied.message.contains(&settings.display().to_string()));
    assert_eq!(denied.details, Some(json!({"bundle_id": BUNDLE})));
}

#[test]
fn a_locked_screen_refuses_the_bind_even_when_the_probe_failed() {
    let env = Env::new();
    env.fake().locked = true;
    assert_eq!(
        bind(&env, BUNDLE).unwrap_err().code,
        ErrorCode::ScreenLocked
    );
}

#[test]
fn several_allowed_matches_are_ambiguous() {
    let env = Env::allowing(&[BUNDLE, "com.example.two"]);
    env.fake().running = vec![
        running(BUNDLE, "Example", 4242),
        running("com.example.two", "Example", 4243),
    ];
    let error = bind(&env, "Example").unwrap_err();
    assert_eq!(
        error,
        ComputerUseError::new(
            ErrorCode::AmbiguousApp,
            "the app spec matched several allowed apps (com.example.app, com.example.two); call \
             get_app with the bundle_id of the one you want"
        )
        .with_details(json!({"bundle_ids": [BUNDLE, "com.example.two"]}))
    );
}

#[test]
fn name_matching_casefolds_unicode() {
    let env = Env::new();
    env.fake().running = vec![running(BUNDLE, "Weiß", 1)];
    assert_eq!(bind(&env, "weiss").unwrap().pid, 1);
    assert_eq!(bind(&env, "WEISS").unwrap().pid, 1);
}

#[test]
fn an_app_that_is_not_running_launches_by_its_gated_bundle_id() {
    let env = Env::new();
    env.fake().running.clear();
    env.fake().launch_result = Some(running(BUNDLE, NAME, 5555));
    let app = bind(&env, BUNDLE).unwrap();
    assert_eq!(env.fake().launch_calls, [BUNDLE]);
    assert_eq!(app.pid, 5555);
}

#[test]
fn a_denied_bundle_never_launches() {
    let env = Env::allowing(&["com.other.app"]);
    env.fake().running.clear();
    assert_eq!(
        bind(&env, "com.denied.app").unwrap_err().code,
        ErrorCode::AppNotAllowed
    );
    assert_eq!(
        bind(&env, BUNDLE).unwrap_err().code,
        ErrorCode::AppNotAllowed
    );
    assert!(env.fake().launch_calls.is_empty());
}

#[test]
fn a_denied_resolved_name_never_launches() {
    let env = Env::allowing(&["com.other.app"]);
    env.fake().running.clear();
    env.fake().bundle_for_name = Ok(Some(BUNDLE.to_string()));
    assert_eq!(
        bind(&env, "Example").unwrap_err().code,
        ErrorCode::AppNotAllowed
    );
    assert!(env.fake().launch_calls.is_empty());
}

#[test]
fn an_unresolvable_name_fails_closed_without_launching() {
    let env = Env::new();
    env.fake().running.clear();
    let error = bind(&env, "Mystery App").unwrap_err();
    let settings = env
        .agent_dir
        .path()
        .join("settings")
        .join("computer-use.toml");
    assert_eq!(
        error,
        ComputerUseError::new(
            ErrorCode::AppNotAllowed,
            format!(
                "could not resolve 'Mystery App' to a bundle id without launching it, so Prime \
                 Agent fails closed; ask the user to add the app's bundle id to `apps.allowed` in \
                 {} and call get_app with {{'bundle_id': ...}}",
                settings.display()
            )
        )
        .with_details(json!({"spec": "Mystery App"}))
    );
    assert!(env.fake().launch_calls.is_empty());
}

#[test]
fn an_unreadable_path_fails_closed_without_launching() {
    let env = Env::new();
    env.fake().running.clear();
    let spec = AppSpec::dict("path", "/nonexistent/app.app");
    assert_eq!(
        env.session.get_app(&spec, None).unwrap_err().code,
        ErrorCode::AppNotAllowed
    );
    assert!(env.fake().launch_calls.is_empty());
}

#[test]
fn an_allowed_resolved_name_launches_once_by_bundle_id() {
    let env = Env::new();
    env.fake().running.clear();
    env.fake().bundle_for_name = Ok(Some(BUNDLE.to_string()));
    let app = bind(&env, "Example").unwrap();
    assert_eq!(env.fake().launch_calls, [BUNDLE]);
    assert_eq!(app.bundle_id, BUNDLE);
}

#[test]
fn a_dotted_display_name_resolves_before_the_launch() {
    // "Acme 1.0" looks like a bundle id but Spotlight resolves it as a name.
    let env = Env::new();
    env.fake().running.clear();
    env.fake().bundle_for_name = Ok(Some(BUNDLE.to_string()));
    assert_eq!(bind(&env, "Acme 1.0").unwrap().bundle_id, BUNDLE);
    assert_eq!(env.fake().launch_calls, [BUNDLE]);
}

#[test]
fn an_unresolvable_dotted_string_stays_a_bundle_id() {
    let env = Env::allowing(&["com.mystery.app"]);
    env.fake().running.clear();
    env.fake().launch_result = Some(running("com.mystery.app", "Mystery", 9999));
    assert_eq!(
        bind(&env, "com.mystery.app").unwrap().bundle_id,
        "com.mystery.app"
    );
    assert_eq!(env.fake().launch_calls, ["com.mystery.app"]);
}

#[test]
fn a_launch_opening_another_bundle_is_refused() {
    let env = Env::new();
    env.fake().running.clear();
    env.fake().launch_result = Some(running("com.evil.app", "Evil", 6666));
    let error = bind(&env, BUNDLE).unwrap_err();
    assert_eq!(
        error.details,
        Some(json!({"gated_bundle_id": BUNDLE, "launched_bundle_id": "com.evil.app"}))
    );
}

#[test]
fn a_missing_or_unknown_accessibility_grant_refuses_the_bind() {
    for state in [PermissionState::Missing, PermissionState::Unknown] {
        let env = Env::new();
        env.fake().accessibility = state;
        let error = bind(&env, BUNDLE).unwrap_err();
        assert_eq!(error.code, ErrorCode::PermissionsNotGranted);
        assert!(error.message.contains("Accessibility"));
    }
}

#[test]
fn a_missing_grant_never_launches_the_app() {
    let env = Env::new();
    env.fake().accessibility = PermissionState::Missing;
    env.fake().running.clear();
    assert_eq!(
        bind(&env, BUNDLE).unwrap_err().code,
        ErrorCode::PermissionsNotGranted
    );
    assert!(env.fake().launch_calls.is_empty());
}

#[test]
fn the_bind_revalidates_under_the_guard_after_a_launch() {
    // The app launches, then quits (or is revoked) before the first read.
    let env = Env::new();
    env.fake().running.clear();
    env.fake().after_launch = Some(|state| state.running.clear());
    assert_eq!(
        bind(&env, BUNDLE).unwrap_err().code,
        ErrorCode::AppNotRunning
    );
}

#[test]
fn malformed_specs_are_invalid_arguments() {
    let env = Env::new();
    assert_eq!(
        bind(&env, "   ").unwrap_err(),
        crate::error::invalid("the app spec must not be empty").with_details(json!({"spec": ""}))
    );
    let two_keys = AppSpec {
        shape: crate::spec::SpecShape::Dict {
            entries: vec![
                ("bundle_id".to_string(), Some("a".to_string())),
                ("name".to_string(), Some("b".to_string())),
            ],
            keys: vec![json!("bundle_id"), json!("name")],
        },
        display: "{'bundle_id': 'a', 'name': 'b'}".to_string(),
        repr: "{'bundle_id': 'a', 'name': 'b'}".to_string(),
    };
    assert_eq!(
        env.session.get_app(&two_keys, None).unwrap_err(),
        crate::error::invalid("a dict app spec must carry exactly one of bundle_id, name, or path")
            .with_details(json!({"keys": ["bundle_id", "name"]}))
    );
    let not_a_string = AppSpec {
        shape: crate::spec::SpecShape::Dict {
            entries: vec![("name".to_string(), None)],
            keys: vec![json!("name")],
        },
        display: "{'name': 5}".to_string(),
        repr: "{'name': 5}".to_string(),
    };
    assert_eq!(
        env.session.get_app(&not_a_string, None).unwrap_err(),
        crate::error::invalid("name must be a non-empty string")
            .with_details(json!({"kind": "name"}))
    );
    let other = AppSpec {
        shape: crate::spec::SpecShape::Other("int".to_string()),
        display: "5".to_string(),
        repr: "5".to_string(),
    };
    assert_eq!(
        env.session.get_app(&other, None).unwrap_err(),
        crate::error::invalid("the app spec must be a string or a dict, got int")
            .with_details(json!({"spec": "int"}))
    );
}

#[test]
fn per_app_instructions_append_once_per_host() {
    let env = Env::new();
    let guides = tempfile::tempdir().unwrap();
    std::fs::write(
        guides.path().join("com.example.app.md"),
        "  Use the sidebar.\n",
    )
    .unwrap();
    let app: BoundApp = env
        .session
        .get_app(&AppSpec::text(BUNDLE), Some(guides.path().to_path_buf()))
        .unwrap();
    assert!(
        app.state
            .as_deref()
            .unwrap()
            .ends_with("\nUse the sidebar.")
    );
    let full = call(&env, &app, AppCall::GetAxState { diff: false }).unwrap();
    assert!(!full.as_str().unwrap().contains("Use the sidebar."));
}

#[test]
fn instruction_files_are_keyed_by_the_sanitized_bundle_id() {
    let guides = tempfile::tempdir().unwrap();
    std::fs::write(
        guides.path().join("com.tinyspeck.slackmacgap.md"),
        "slack guide",
    )
    .unwrap();
    std::fs::write(guides.path().join("we_rd__app.md"), "sanitized").unwrap();
    std::fs::write(guides.path().join("empty.md"), "  \n").unwrap();
    let load = |bundle| crate::session::app::load_instructions(Some(guides.path()), bundle);
    assert_eq!(
        load("com.tinyspeck.slackmacgap").as_deref(),
        Some("slack guide")
    );
    assert_eq!(load("we/rd *app").as_deref(), Some("sanitized"));
    assert_eq!(load("slack"), None);
    assert_eq!(load("empty"), None);
    assert_eq!(crate::session::app::load_instructions(None, "slack"), None);
}

#[test]
fn the_shipped_guides_resolve_by_their_bundle_ids() {
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../skills/computer-use/references/app-instructions");
    let load = |bundle| crate::session::app::load_instructions(Some(&shipped), bundle);
    assert!(load("com.tinyspeck.slackmacgap").is_some());
    assert!(load("notion.id").is_some());
    assert_eq!(load("slack"), None);
    assert_eq!(load("notion"), None);
}
