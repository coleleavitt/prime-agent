//! The App-level parity tests: ported from the skill's `tests/test_api.py`,
//! `test_w1_security.py` and `test_w5_core.py` (their `AppEnvironment`
//! cases), run against [`super::fake::FakePlatform`].

mod bind;
mod dispatch;
mod elements;
mod paste;
mod screenshots;
mod security;
mod state;

use super::fake::{Env, FakePlatform, BUNDLE};
use super::{AppCall, BoundApp, IndexArg, PointArg, TargetArg, TextArg};
use crate::error::{ComputerUseError, ErrorCode, Result};
use crate::platform::{MouseButton, Platform};
use crate::spec::AppSpec;
use serde_json::Value;

/// Bind the example app through the public `get_app`.
fn bind<P: Platform>(env: &Env<P>, spec: &str) -> Result<BoundApp> {
    env.session.get_app(&AppSpec::text(spec), None)
}

fn bound(env: &Env<FakePlatform>) -> BoundApp {
    bind(env, BUNDLE).expect("the example app binds")
}

fn call<P: Platform>(env: &Env<P>, app: &BoundApp, call: AppCall) -> Result<Value> {
    env.session.call(app.handle, call)
}

fn point(x: f64, y: f64) -> PointArg {
    PointArg::Valid {
        x,
        y,
        repr: format!("({x:?}, {y:?})"),
    }
}

fn click_index(index: i64) -> AppCall {
    AppCall::Click {
        target: TargetArg::Index(index),
        button: MouseButton::Left,
        count: 1,
    }
}

fn click_point(x: f64, y: f64) -> AppCall {
    AppCall::Click {
        target: TargetArg::Point(point(x, y)),
        button: MouseButton::Left,
        count: 1,
    }
}

fn text(value: &str) -> TextArg {
    TextArg::Text(value.to_string())
}

#[track_caller]
fn code(result: Result<Value>) -> ErrorCode {
    result.expect_err("the call fails").code
}

#[track_caller]
fn error(result: Result<Value>) -> ComputerUseError {
    result.expect_err("the call fails")
}
