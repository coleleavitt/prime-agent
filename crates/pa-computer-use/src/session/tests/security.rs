//! Secure-focus refusals and gate ordering (`test_api`'s secure-field
//! dispatch cases; `test_w1`'s `LiveSecureFocus`, `SecureFocusFailClosed` and
//! `GateOrdering` cases).

use serde_json::json;

use super::*;
use crate::session::fake::{Call, PID};

#[test]
fn typing_into_a_live_secure_focus_is_refused_with_the_hand_off() {
    let env = Env::new();
    env.fake().focused_index = Some(4);
    env.fake().secure_focus = Some(true);
    let app = bound(&env);
    let refused = error(call(
        &env,
        &app,
        AppCall::TypeText {
            text: text("hunter2"),
        },
    ));
    assert_eq!(refused.code, ErrorCode::ActionUnsupported);
    assert!(refused.message.contains("ask the user"));
    assert_eq!(refused.details, Some(json!({"live": true})));
    assert_eq!(
        code(call(&env, &app, AppCall::PressKey { key: text("a") })),
        ErrorCode::ActionUnsupported
    );
    assert!(env.fake().calls.is_empty());
}

#[test]
fn the_live_focus_wins_over_the_snapshot() {
    // The snapshot's focus is the plain Search field; the live focus moved
    // onto a secure field.
    let env = Env::new();
    env.fake().focused_index = Some(1);
    env.fake().secure_focus = Some(true);
    let app = bound(&env);
    assert_eq!(
        code(call(
            &env,
            &app,
            AppCall::TypeText {
                text: text("hunter2")
            }
        )),
        ErrorCode::ActionUnsupported
    );
    // Focus moved off the snapshot's secure field: typing proceeds.
    let env = Env::new();
    env.fake().focused_index = Some(4);
    env.fake().secure_focus = Some(false);
    let app = bound(&env);
    call(
        &env,
        &app,
        AppCall::TypeText {
            text: text("hello"),
        },
    )
    .unwrap();
    assert_eq!(
        env.fake().calls,
        [Call::TypeText {
            pid: PID,
            text: "hello".to_string()
        }]
    );
}

#[test]
fn an_unreadable_live_focus_fails_closed() {
    for focused in [Some(4), Some(1), None] {
        let env = Env::new();
        env.fake().focused_index = focused;
        env.fake().secure_focus = None;
        let app = bound(&env);
        let refused = error(call(&env, &app, AppCall::PressKey { key: text("a") }));
        assert_eq!(refused.code, ErrorCode::ActionUnsupported);
        assert!(refused.message.contains("could not verify"));
        assert_eq!(refused.details, Some(json!({"live": false})));
        assert!(env.fake().calls.is_empty());
    }
}

#[test]
fn a_locked_screen_and_a_revoked_allowlist_win_over_the_secure_refusal() {
    let env = Env::new();
    env.fake().focused_index = Some(4);
    env.fake().secure_focus = Some(true);
    let app = bound(&env);
    env.fake().locked = true;
    assert_eq!(
        code(call(&env, &app, AppCall::PressKey { key: text("a") })),
        ErrorCode::ScreenLocked
    );
    env.fake().locked = false;
    env.allow_only(&[]);
    assert_eq!(
        code(call(
            &env,
            &app,
            AppCall::TypeText {
                text: text("secret")
            }
        )),
        ErrorCode::AppNotAllowed
    );
    assert!(env.fake().calls.is_empty());
}

#[test]
fn the_secure_refusal_runs_before_the_chord_is_parsed() {
    let env = Env::new();
    env.fake().secure_focus = Some(true);
    let app = bound(&env);
    assert_eq!(
        code(call(
            &env,
            &app,
            AppCall::PressKey {
                key: text("notakey")
            }
        )),
        ErrorCode::ActionUnsupported
    );
}
