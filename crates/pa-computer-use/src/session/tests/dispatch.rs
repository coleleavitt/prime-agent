//! Input dispatch, the guard, telemetry and the settle (`test_api`'s
//! `AppDispatchTests` and `SettleTests`, `test_w5`'s activate/frontmost/settle
//! cases, `test_w1`'s stale-pid and click-count cases).

use serde_json::json;

use super::*;
use crate::element::Element;
use crate::keymap::parse_chord;
use crate::platform::ScrollDirection;
use crate::session::fake::{AxCall, Call, PID};
use crate::telemetry::Outcome;

#[test]
fn a_single_left_click_on_a_press_element_uses_the_ax_action() {
    let env = Env::new();
    let app = bound(&env);
    call(&env, &app, click_index(2)).unwrap();
    assert_eq!(
        env.fake().ax_calls,
        [AxCall::Perform {
            title: Some("Save".to_string()),
            action: "AXPress".to_string()
        }]
    );
    assert!(env.fake().calls.is_empty());
}

#[test]
fn an_element_without_press_clicks_its_screen_space_center() {
    let env = Env::new();
    let app = bound(&env);
    call(&env, &app, click_index(0)).unwrap();
    assert_eq!(
        env.fake().calls,
        [Call::Click {
            pid: PID,
            point: (120.0, 70.0),
            button: MouseButton::Left,
            count: 1
        }]
    );
}

#[test]
fn a_double_click_bypasses_the_press_action() {
    let env = Env::new();
    let app = bound(&env);
    let double = AppCall::Click {
        target: TargetArg::Index(2),
        button: MouseButton::Left,
        count: 2,
    };
    call(&env, &app, double).unwrap();
    assert_eq!(
        env.fake().calls,
        [Call::Click {
            pid: PID,
            point: (320.0, 102.0),
            button: MouseButton::Left,
            count: 2
        }]
    );
    assert!(env.fake().ax_calls.is_empty());
}

#[test]
fn window_points_translate_by_the_window_origin() {
    let env = Env::new();
    let app = bound(&env);
    call(&env, &app, click_point(10.0, 20.0)).unwrap();
    let drag = AppCall::Drag {
        from: point(10.0, 20.0),
        to: point(30.0, 40.0),
    };
    call(&env, &app, drag).unwrap();
    assert_eq!(
        env.fake().calls,
        [
            Call::Click {
                pid: PID,
                point: (110.0, 70.0),
                button: MouseButton::Left,
                count: 1
            },
            Call::Drag {
                pid: PID,
                start: (110.0, 70.0),
                end: (130.0, 90.0)
            }
        ]
    );
}

#[test]
fn scrolls_carry_the_element_center_or_the_translated_point() {
    let env = Env::new();
    let app = bound(&env);
    let by_index = AppCall::Scroll {
        target: TargetArg::Index(0),
        direction: ScrollDirection::Down,
        pages: 2,
    };
    let by_point = AppCall::Scroll {
        target: TargetArg::Point(point(5.0, 5.0)),
        direction: ScrollDirection::Up,
        pages: 1,
    };
    call(&env, &app, by_index).unwrap();
    call(&env, &app, by_point).unwrap();
    assert_eq!(
        env.fake().calls,
        [
            Call::Scroll {
                pid: PID,
                direction: ScrollDirection::Down,
                pages: 2,
                point: (120.0, 70.0)
            },
            Call::Scroll {
                pid: PID,
                direction: ScrollDirection::Up,
                pages: 1,
                point: (105.0, 55.0)
            }
        ]
    );
}

#[test]
fn stale_indices_raise_element_stale_without_dispatch() {
    let env = Env::new();
    let app = bound(&env);
    for stale in [999, -1] {
        assert_eq!(
            code(call(&env, &app, click_index(stale))),
            ErrorCode::ElementStale
        );
    }
    assert!(env.fake().calls.is_empty());
}

#[test]
fn an_invalid_click_target_is_refused_after_the_guard() {
    let env = Env::new();
    let app = bound(&env);
    let invalid = AppCall::Click {
        target: TargetArg::Invalid("str".to_string()),
        button: MouseButton::Left,
        count: 1,
    };
    assert_eq!(
        error(call(&env, &app, invalid.clone())),
        crate::error::invalid("target must be an element index or an (x, y) tuple, got str")
            .with_details(json!({"target": "str"}))
    );
    env.fake().locked = true;
    assert_eq!(code(call(&env, &app, invalid)), ErrorCode::ScreenLocked);
}

#[test]
fn key_chords_and_text_dispatch_to_the_bound_pid() {
    let env = Env::new();
    let app = bound(&env);
    call(
        &env,
        &app,
        AppCall::PressKey {
            key: text("cmd+shift+f"),
        },
    )
    .unwrap();
    call(
        &env,
        &app,
        AppCall::TypeText {
            text: text("hello"),
        },
    )
    .unwrap();
    call(&env, &app, AppCall::TypeText { text: text("") }).unwrap();
    assert_eq!(
        env.fake().calls,
        [
            Call::PressKey {
                pid: PID,
                chord: parse_chord("cmd+shift+f").unwrap()
            },
            Call::TypeText {
                pid: PID,
                text: "hello".to_string()
            }
        ]
    );
}

#[test]
fn invalid_chords_and_non_string_text_are_invalid_arguments() {
    let env = Env::new();
    let app = bound(&env);
    assert_eq!(
        code(call(
            &env,
            &app,
            AppCall::PressKey {
                key: text("cmd++c")
            }
        )),
        ErrorCode::InvalidArgument
    );
    assert_eq!(
        error(call(
            &env,
            &app,
            AppCall::PressKey {
                key: TextArg::NotText("int".to_string())
            }
        )),
        crate::error::invalid("key chord must be a string, got int")
            .with_details(json!({"key": "int"}))
    );
    assert_eq!(
        error(call(
            &env,
            &app,
            AppCall::TypeText {
                text: TextArg::NotText("int".to_string())
            }
        )),
        crate::error::invalid("text must be a string, got int")
            .with_details(json!({"text": "int"}))
    );
    assert!(env.fake().calls.is_empty());
}

#[test]
fn element_drift_raises_stale_naming_the_change() {
    let env = Env::new();
    let app = bound(&env);
    env.fake().drift = true;
    let stale = error(call(&env, &app, click_index(2)));
    assert_eq!(
        stale,
        ComputerUseError::new(
            ErrorCode::ElementStale,
            "element 2 changed since the last observation ('AXButton' -> 'AXGhost'); re-observe \
             with get_ax_state()"
        )
        .with_details(json!({"element_index": 2}))
    );
}

#[test]
fn points_outside_the_observed_window_are_rejected() {
    let env = Env::new();
    let app = bound(&env);
    let outside = error(call(&env, &app, click_point(400.0, 150.0)));
    assert_eq!(
        outside,
        crate::error::invalid(
            "point (400.0, 150.0) is outside the observed window (400x300); use coordinates from \
             its screenshot"
        )
        .with_details(json!({"point": "(400.0, 150.0)"}))
    );
    let malformed = AppCall::Click {
        target: TargetArg::Point(PointArg::Invalid {
            repr: "(1,)".to_string(),
        }),
        button: MouseButton::Left,
        count: 1,
    };
    assert_eq!(
        error(call(&env, &app, malformed)),
        crate::error::invalid("point must be an (x, y) pair of numbers, got (1,)")
            .with_details(json!({"point": "(1,)"}))
    );
    assert!(env.fake().calls.is_empty());
}

#[test]
fn an_element_without_geometry_names_the_gap() {
    let env = Env::new();
    env.fake().tree = vec![Element {
        role: Some("AXGroup".to_string()),
        ..Element::default()
    }];
    let app = bound(&env);
    let error = error(call(&env, &app, click_index(0)));
    assert_eq!(error.code, ErrorCode::ActionUnsupported);
    assert!(error.message.contains("has no on-screen position"));
}

#[test]
fn the_guard_rechecks_the_allowlist_the_lock_and_the_grant_before_every_action() {
    let env = Env::new();
    let app = bound(&env);
    env.allow_only(&[]);
    assert_eq!(
        code(call(&env, &app, AppCall::GetAxState { diff: true })),
        ErrorCode::AppNotAllowed
    );
    assert_eq!(
        code(call(&env, &app, click_index(0))),
        ErrorCode::AppNotAllowed
    );
    env.allow_only(&[BUNDLE]);
    env.fake().locked = true;
    assert_eq!(
        code(call(&env, &app, AppCall::GetAxState { diff: true })),
        ErrorCode::ScreenLocked
    );
    assert_eq!(
        code(call(&env, &app, click_index(0))),
        ErrorCode::ScreenLocked
    );
    env.fake().locked = false;
    env.fake().accessibility = crate::permissions::PermissionState::Missing;
    let revoked = error(call(&env, &app, click_index(0)));
    assert_eq!(revoked.code, ErrorCode::PermissionsNotGranted);
    assert!(revoked.message.contains("revoked"));
    assert!(env.fake().calls.is_empty());
}

#[test]
fn the_guard_rejects_a_vanished_or_reused_pid() {
    let env = Env::new();
    let app = bound(&env);
    env.fake().running.clear();
    assert_eq!(
        error(call(&env, &app, click_index(0))),
        ComputerUseError::new(
            ErrorCode::AppNotRunning,
            "pid 4242 is no longer a running app; call get_app again to re-bind it"
        )
        .with_details(json!({"pid": 4242}))
    );
    env.fake().running = vec![crate::platform::RunningApp {
        bundle_id: "com.other.owner".to_string(),
        name: "Other".to_string(),
        pid: PID,
        path: None,
    }];
    assert_eq!(
        error(call(&env, &app, click_index(0))).details,
        Some(json!({"pid": 4242, "running_bundle_id": "com.other.owner"}))
    );
    assert!(env.fake().calls.is_empty());
}

#[test]
fn actions_emit_one_event_with_the_outcome_and_the_error_code() {
    let env = Env::new();
    let app = bound(&env);
    call(&env, &app, click_index(0)).unwrap();
    let _ = call(&env, &app, click_index(999));
    assert_eq!(
        env.actions(),
        [
            ("click", Outcome::Ok),
            ("click", Outcome::Error(ErrorCode::ElementStale))
        ]
    );
}

#[test]
fn a_guard_failure_of_get_ax_state_emits_no_event() {
    let env = Env::new();
    let app = bound(&env);
    env.fake().locked = true;
    let _ = call(&env, &app, AppCall::GetAxState { diff: true });
    assert!(env.actions().is_empty());
}

#[test]
fn click_counts_up_to_ten_reach_the_backend() {
    let env = Env::new();
    let app = bound(&env);
    let ten = AppCall::Click {
        target: TargetArg::Index(0),
        button: MouseButton::Left,
        count: 10,
    };
    call(&env, &app, ten).unwrap();
    assert!(matches!(
        env.fake().calls[..],
        [Call::Click { count: 10, .. }]
    ));
}

#[test]
fn activate_dispatches_with_the_bound_pid_and_emits_its_event() {
    let env = Env::new();
    let app = bound(&env);
    call(&env, &app, AppCall::Activate).unwrap();
    assert_eq!(env.fake().calls, [Call::Activate { pid: PID }]);
    assert_eq!(env.actions(), [("activate", Outcome::Ok)]);
    env.fake().running.clear();
    assert_eq!(
        code(call(&env, &app, AppCall::Activate)),
        ErrorCode::AppNotRunning
    );
    assert_eq!(env.fake().calls.len(), 1);
}

#[test]
fn is_frontmost_compares_the_frontmost_pid() {
    let env = Env::new();
    let app = bound(&env);
    assert_eq!(
        call(&env, &app, AppCall::IsFrontmost).unwrap(),
        json!(false)
    );
    env.fake().frontmost = Some(PID + 1);
    assert_eq!(
        call(&env, &app, AppCall::IsFrontmost).unwrap(),
        json!(false)
    );
    env.fake().frontmost = Some(PID);
    assert_eq!(call(&env, &app, AppCall::IsFrontmost).unwrap(), json!(true));
    assert!(env.actions().is_empty());
}

#[test]
fn an_injected_action_settles_until_two_reads_agree() {
    let env = Env::new();
    let app = bound(&env);
    let fingerprint = |value: &str| vec![Some("Main".to_string()), Some(value.to_string())];
    env.fake().fingerprints = Some(vec![fingerprint("5"), fingerprint("6"), fingerprint("6")]);
    call(&env, &app, click_index(0)).unwrap();
    assert!(env.fake().fingerprint_reads >= 3);
}

#[test]
fn a_churning_app_settles_at_the_cap() {
    let env = Env::new();
    let app = bound(&env);
    let fingerprint = |value: &str| vec![Some("loading".to_string()), Some(value.to_string())];
    env.fake().fingerprints = Some(vec![fingerprint("1"), fingerprint("2")]);
    let started = std::time::Instant::now();
    call(&env, &app, click_index(0)).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(env.fake().fingerprint_reads >= 3);
}

#[test]
fn an_unreadable_fingerprint_settles_at_once_and_a_failed_action_never_settles() {
    let env = Env::new();
    let app = bound(&env);
    call(
        &env,
        &app,
        AppCall::TypeText {
            text: text("hello"),
        },
    )
    .unwrap();
    assert_eq!(env.fake().fingerprint_reads, 1);
    let _ = call(&env, &app, click_index(999));
    assert_eq!(env.fake().fingerprint_reads, 1);
}

#[test]
fn an_unknown_handle_is_app_not_running() {
    let env = Env::new();
    let error = env.session.call(77, AppCall::Activate).unwrap_err();
    assert_eq!(error.code, ErrorCode::AppNotRunning);
    assert_eq!(error.details, Some(json!({"handle": 77})));
}
