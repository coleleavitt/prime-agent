//! Element actions and observation (`test_api`'s `AppElementActionTests` and
//! `AppObservationTests`, `test_w1`'s `select_text` failure modes).

use serde_json::json;

use super::*;
use crate::secure::SECURE_HANDOFF;
use crate::session::fake::AxCall;
use crate::session::ActionArg;
use crate::testing::{element, with_changed_value};

fn set_value(index: i64, value: &str) -> AppCall {
    AppCall::SetValue {
        index: IndexArg::Valid(index),
        value: value.to_string(),
    }
}

fn select(index: i64, text: &str, prefix: Option<&str>, suffix: Option<&str>) -> AppCall {
    AppCall::SelectText {
        index: IndexArg::Valid(index),
        text: text.to_string(),
        prefix: prefix.map(ToString::to_string),
        suffix: suffix.map(ToString::to_string),
    }
}

#[test]
fn set_value_writes_an_editable_element() {
    let env = Env::new();
    let app = bound(&env);
    call(&env, &app, set_value(1, "typed")).unwrap();
    assert_eq!(
        env.fake().ax_calls,
        [AxCall::SetValue {
            title: Some("Search".to_string()),
            value: "typed".to_string()
        }]
    );
}

#[test]
fn set_value_refuses_a_secure_field_with_the_hand_off() {
    let env = Env::new();
    let app = bound(&env);
    assert_eq!(
        error(call(&env, &app, set_value(4, "secret"))),
        crate::error::unsupported(format!("element 4: {SECURE_HANDOFF}"))
            .with_details(json!({"element_index": 4, "secure": true}))
    );
    assert!(env.fake().ax_calls.is_empty());
}

#[test]
fn set_value_refuses_a_non_editable_element() {
    let env = Env::new();
    env.fake().settable = false;
    let app = bound(&env);
    assert_eq!(
        code(call(&env, &app, set_value(1, "typed"))),
        ErrorCode::ActionUnsupported
    );
    assert!(env.fake().ax_calls.is_empty());
}

#[test]
fn set_value_and_select_text_refuse_a_field_that_turned_secure_after_the_snapshot() {
    let env = Env::new();
    env.fake().live_secure = Some(true);
    let app = bound(&env);
    let refused = error(call(&env, &app, set_value(1, "secret")));
    assert!(refused.message.contains("secure field"));
    assert_eq!(
        code(call(&env, &app, select(1, "que", None, None))),
        ErrorCode::ActionUnsupported
    );
    assert!(env.fake().ax_calls.is_empty());
}

#[test]
fn an_unverifiable_live_field_refuses_the_write() {
    let env = Env::new();
    env.fake().live_secure = None;
    let app = bound(&env);
    assert_eq!(
        error(call(&env, &app, set_value(1, "x"))).details,
        Some(json!({"element_index": 1}))
    );
}

#[test]
fn select_text_sets_the_range_of_the_one_occurrence() {
    let env = Env::new();
    let app = bound(&env);
    call(&env, &app, select(1, "ue", None, None)).unwrap();
    call(&env, &app, select(1, "er", Some("u"), Some("y"))).unwrap();
    let range = |location, length| AxCall::SelectRange {
        title: Some("Search".to_string()),
        location,
        length,
    };
    assert_eq!(env.fake().ax_calls, [range(1, 2), range(2, 2)]);
}

#[test]
fn select_text_on_a_secure_field_refuses() {
    let env = Env::new();
    let app = bound(&env);
    assert_eq!(
        code(call(&env, &app, select(4, "secret", None, None))),
        ErrorCode::ActionUnsupported
    );
    assert!(env.fake().ax_calls.is_empty());
}

#[test]
fn select_text_failure_modes() {
    let env = Env::new();
    env.fake().tree = vec![
        element("AXTextArea", Some("Notes"), Some("ab cd ab")),
        element("AXTextArea", Some("Empty"), None),
    ];
    let app = bound(&env);
    assert_eq!(
        error(call(&env, &app, select(0, "nope", None, None))),
        ComputerUseError::new(
            ErrorCode::ElementStale,
            "'nope' is not in the element's current text; re-observe with get_ax_state()"
        )
        .with_details(json!({"element_index": 0}))
    );
    assert_eq!(
        error(call(&env, &app, select(0, "ab", None, None))),
        crate::error::unsupported("'ab' occurs 2 times; disambiguate it with prefix and suffix")
            .with_details(json!({"element_index": 0, "occurrences": 2}))
    );
    assert_eq!(
        error(call(&env, &app, select(1, "ab", None, None))),
        crate::error::unsupported("element 1 has no readable text to search")
            .with_details(json!({"element_index": 1}))
    );
    assert!(env.fake().ax_calls.is_empty());
}

#[test]
fn an_invalid_element_index_is_refused() {
    let env = Env::new();
    let app = bound(&env);
    let call_with = AppCall::SetValue {
        index: IndexArg::Invalid("str".to_string()),
        value: "x".to_string(),
    };
    assert_eq!(
        error(call(&env, &app, call_with)),
        crate::error::invalid("element index must be an integer, got str")
            .with_details(json!({"element_index": "str"}))
    );
}

#[test]
fn a_secondary_action_must_be_exposed() {
    let env = Env::new();
    let app = bound(&env);
    let secondary = |name: &str| AppCall::SecondaryAction {
        index: IndexArg::Valid(3),
        action: ActionArg::Name(name.to_string()),
    };
    assert_eq!(
        error(call(&env, &app, secondary("AXShowMenu"))),
        crate::error::unsupported("element 3 exposes AXPress, not AXShowMenu")
            .with_details(json!({"element_index": 3, "action": "AXShowMenu"}))
    );
    call(&env, &app, secondary("AXPress")).unwrap();
    assert_eq!(
        env.fake().ax_calls,
        [AxCall::Perform {
            title: Some("Enabled".to_string()),
            action: "AXPress".to_string()
        }]
    );
    let unnamed = AppCall::SecondaryAction {
        index: IndexArg::Valid(0),
        action: ActionArg::NotText("5".to_string()),
    };
    assert_eq!(
        error(call(&env, &app, unnamed)).message,
        "element 0 exposes no actions, not 5"
    );
}

#[test]
fn get_ax_state_diffs_against_the_previous_snapshot() {
    let env = Env::new();
    let app = bound(&env);
    assert_eq!(
        call(&env, &app, AppCall::GetAxState { diff: true }).unwrap(),
        json!("(no changes since the previous observation)")
    );
    env.fake().tree = with_changed_value(crate::testing::small_tree(), "Search", "new query");
    let changed = call(&env, &app, AppCall::GetAxState { diff: true }).unwrap();
    assert_eq!(
        changed,
        json!(
            "~[1] AXTextField 'Search' = 'new query' placeholder='Search…' (actions: AXSetValue) \
             @ (20, 90) 240x24"
        )
    );
    let full = call(&env, &app, AppCall::GetAxState { diff: false }).unwrap();
    assert!(full.as_str().unwrap().contains("indices [0]..[4]"));
    assert_eq!(
        env.actions(),
        [
            ("get_state", crate::telemetry::Outcome::Ok),
            ("get_state", crate::telemetry::Outcome::Ok),
            ("get_state", crate::telemetry::Outcome::Ok)
        ]
    );
}

#[test]
fn the_header_names_windowless_and_truncated_observations() {
    let env = Env::new();
    env.fake().tree.clear();
    env.fake().window_title = None;
    let app = bound(&env);
    assert_eq!(
        app.state.as_deref(),
        Some("Example (com.example.app) — no focused window")
    );
    env.fake().tree = vec![element("AXGroup", None, None)];
    env.fake().window_title = Some("Doc".to_string());
    env.fake().truncated = true;
    assert_eq!(
        call(&env, &app, AppCall::GetAxState { diff: false }).unwrap(),
        json!(
            "Example (com.example.app) — window 'Doc' — 1 elements, indices [0]..[0] — \
             TRUNCATED: the observation stopped at its element/depth/time bounds, some controls \
             are hidden\n[0] AXGroup"
        )
    );
}
