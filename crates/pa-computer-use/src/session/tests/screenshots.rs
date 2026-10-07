//! Screenshots, the Retina scale, OCR (`test_api`'s screenshot, `MovedWindow`
//! and `RetinaClick` cases; `test_w1`'s window-id plumbing, unknown-grant and
//! snapshot-consistency cases).

use serde_json::json;

use super::*;
use crate::element::Rect;
use crate::permissions::PermissionState;
use crate::platform::{CaptureRequest, RecognizedText};
use crate::session::fake::{Call, PID};

fn retina(env: &Env<FakePlatform>) {
    let mut fake = env.fake();
    fake.screenshot.width = 800;
    fake.screenshot.height = 600;
}

fn clicks(env: &Env<FakePlatform>) -> Vec<(f64, f64)> {
    env.fake()
        .calls
        .iter()
        .filter_map(|call| match call {
            Call::Click { point, .. } => Some(*point),
            _ => None,
        })
        .collect()
}

#[test]
fn the_screenshot_captures_the_window_by_its_id() {
    let env = Env::new();
    let app = bound(&env);
    let shot = call(&env, &app, AppCall::GetScreenshot).unwrap();
    assert_eq!(
        shot,
        json!({"path": "/tmp/computer-use-fake.png", "width": 400, "height": 300})
    );
    assert_eq!(
        env.fake().calls,
        [Call::Screenshot(CaptureRequest::Region {
            origin: (100, 50),
            size: (400, 300),
            window_id: Some(4321)
        })]
    );
    // Capture is not an action: no telemetry.
    assert!(env.actions().is_empty());
}

#[test]
fn a_window_without_an_id_is_never_region_captured() {
    let env = Env::new();
    env.fake().window_id = None;
    let app = bound(&env);
    let refused = error(call(&env, &app, AppCall::GetScreenshot));
    assert_eq!(refused.code, ErrorCode::TransportError);
    assert!(refused.message.contains("cannot be scoped"));
    assert!(env.fake().calls.is_empty());
}

#[test]
fn a_missing_or_unknown_screen_recording_grant_refuses_the_capture() {
    for state in [PermissionState::Missing, PermissionState::Unknown] {
        let env = Env::new();
        env.fake().screen_recording = state;
        let app = bound(&env);
        assert_eq!(
            error(call(&env, &app, AppCall::GetScreenshot)).details,
            Some(json!({"permission": "screen_recording", "reported": state.as_str()}))
        );
        assert!(env.fake().calls.is_empty());
    }
}

#[test]
fn no_observed_window_is_a_transport_error() {
    let env = Env::new();
    env.fake().window_rect = None;
    let app = bound(&env);
    let refused = error(call(&env, &app, AppCall::GetScreenshot));
    assert_eq!(refused.code, ErrorCode::TransportError);
    assert!(refused.message.contains("no focused window"));
}

#[test]
fn the_guard_runs_before_the_capture() {
    let env = Env::new();
    let app = bound(&env);
    env.allow_only(&[]);
    assert_eq!(
        code(call(&env, &app, AppCall::GetScreenshot)),
        ErrorCode::AppNotAllowed
    );
    assert!(env.fake().calls.is_empty());
}

#[test]
fn a_reobserve_during_the_capture_cannot_retag_the_shot() {
    let env = Env::new();
    let app = bound(&env);
    env.fake().during_capture = Some(|state| {
        state.window_id = Some(9999);
        state.window_rect = Some(Rect::new(10.0, 10.0, 100.0, 100.0));
    });
    call(&env, &app, AppCall::GetScreenshot).unwrap();
    assert!(matches!(
        env.fake().calls[0],
        Call::Screenshot(CaptureRequest::Region {
            window_id: Some(4321),
            ..
        })
    ));
    // The stored shot keeps the 4321 tag: once the re-observe sees window
    // 9999, the screenshot's points are refused instead of retargeted.
    call(&env, &app, AppCall::GetAxState { diff: true }).unwrap();
    assert_eq!(
        error(call(&env, &app, click_point(10.0, 10.0))).message,
        "the focused window changed since the screenshot; take a fresh screenshot before \
         clicking image coordinates"
    );
}

#[test]
fn a_one_x_capture_maps_window_pixels_one_to_one() {
    let env = Env::new();
    let app = bound(&env);
    call(&env, &app, AppCall::GetScreenshot).unwrap();
    call(&env, &app, click_point(100.0, 50.0)).unwrap();
    assert_eq!(clicks(&env), [(200.0, 100.0)]);
}

#[test]
fn a_two_x_capture_scales_back_to_logical_window_space() {
    let env = Env::new();
    retina(&env);
    let app = bound(&env);
    call(&env, &app, AppCall::GetScreenshot).unwrap();
    call(&env, &app, click_point(400.0, 150.0)).unwrap();
    assert_eq!(clicks(&env), [(300.0, 125.0)]);
    let outside = error(call(&env, &app, click_point(800.0, 150.0)));
    assert_eq!(
        outside,
        crate::error::invalid(
            "point (800.0, 150.0) is outside the captured image (800x600); use coordinates from \
             its screenshot"
        )
        .with_details(json!({"point": "(800.0, 150.0)"}))
    );
}

#[test]
fn the_capture_scale_survives_a_moved_window() {
    let env = Env::new();
    retina(&env);
    let app = bound(&env);
    call(&env, &app, AppCall::GetScreenshot).unwrap();
    env.fake().window_rect = Some(Rect::new(260.0, 12.0, 400.0, 300.0));
    call(&env, &app, AppCall::GetAxState { diff: true }).unwrap();
    call(&env, &app, click_point(400.0, 150.0)).unwrap();
    assert_eq!(clicks(&env), [(460.0, 87.0)]);
}

#[test]
fn a_resized_window_rejects_points_that_no_longer_fit() {
    let env = Env::new();
    retina(&env);
    let app = bound(&env);
    call(&env, &app, AppCall::GetScreenshot).unwrap();
    env.fake().window_rect = Some(Rect::new(100.0, 50.0, 200.0, 150.0));
    call(&env, &app, AppCall::GetAxState { diff: true }).unwrap();
    let error = error(call(&env, &app, click_point(400.0, 100.0)));
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(
        error
            .message
            .contains("lands outside the observed window (200x150)"),
        "{}",
        error.message
    );
}

#[test]
fn a_changed_focused_window_invalidates_the_screenshot_points() {
    let env = Env::new();
    retina(&env);
    let app = bound(&env);
    call(&env, &app, AppCall::GetScreenshot).unwrap();
    env.fake().window_id = Some(7);
    call(&env, &app, AppCall::GetAxState { diff: true }).unwrap();
    assert_eq!(
        error(call(&env, &app, click_point(10.0, 10.0))),
        crate::error::transport(
            "the focused window changed since the screenshot; take a fresh screenshot before \
             clicking image coordinates"
        )
        .with_details(json!({}))
    );
}

#[test]
fn text_regions_scale_to_the_capture_and_carry_its_path() {
    let env = Env::new();
    env.fake().recognized = vec![RecognizedText {
        text: "Save".to_string(),
        confidence: 0.5,
        bbox: (0.5, 0.25, 0.25, 0.5),
    }];
    let app = bound(&env);
    assert_eq!(
        call(&env, &app, AppCall::GetTextRegions).unwrap(),
        json!({
            "regions": [{"text": "Save", "confidence": 0.5, "x": 200.0, "y": 75.0, "width": 100.0, "height": 150.0}],
            "width": 400,
            "height": 300,
            "path": "/tmp/computer-use-fake.png",
        })
    );
    assert_eq!(
        env.fake().calls,
        [Call::Screenshot(CaptureRequest::Region {
            origin: (100, 50),
            size: (400, 300),
            window_id: Some(4321)
        })]
    );
    let _ = PID;
}

#[test]
fn recognition_failures_are_transport_errors_and_no_text_is_no_regions() {
    let env = Env::new();
    let app = bound(&env);
    assert_eq!(
        call(&env, &app, AppCall::GetTextRegions).unwrap()["regions"],
        json!([])
    );
    env.fake().recognize_error = Some("x".repeat(500));
    assert_eq!(
        call(&env, &app, AppCall::GetTextRegions).unwrap_err(),
        crate::error::transport(format!(
            "vision text recognition failed: {}",
            "x".repeat(200)
        ))
    );
}

#[test]
fn coordinates_round_half_to_even_like_python() {
    let env = Env::new();
    env.fake().window_rect = Some(Rect::new(100.5, 50.5, 400.5, 299.5));
    let app = bound(&env);
    call(&env, &app, AppCall::GetScreenshot).unwrap();
    assert_eq!(
        env.fake().calls,
        [Call::Screenshot(CaptureRequest::Region {
            origin: (100, 50),
            size: (400, 300),
            window_id: Some(4321)
        })]
    );
}
