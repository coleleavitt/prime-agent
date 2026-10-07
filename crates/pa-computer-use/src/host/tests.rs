//! The wire contract, the no-backend answers, backend selection (`test_wayland`'s
//! `SelectionTests`) and the blocking-thread dispatch (`test_api`'s `OffLoop` cases).

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::permissions::MAC_HELP_LINES;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::process::script::Script;
use crate::session::fake::{quick_timing, Env, RecordingTelemetry, BUNDLE};
use crate::telemetry::{Outcome, TelemetryEvent};

fn context(agent_dir: &std::path::Path, telemetry: Arc<RecordingTelemetry>) -> HostContext {
    HostContext::new(Policy::for_agent_dir(agent_dir), telemetry, quick_timing())
}

fn spec(text: &str) -> Value {
    json!({"kind": "str", "value": text, "str": text, "repr": format!("'{text}'")})
}

#[test]
fn requests_route_to_the_backend_and_round_trip_an_app_call() {
    let env = Env::new();
    let backend: &dyn Backend = &env.session;
    let bound = route(
        Some(backend),
        &context(env.agent_dir.path(), Arc::default()),
        "computer_use.get_app",
        &json!({"spec": spec(BUNDLE), "instructions_dir": null}),
    )
    .unwrap();
    assert_eq!(bound["bundle_id"], json!(BUNDLE));
    assert_eq!(bound["pid"], json!(4242));
    let handle = bound["handle"].clone();
    let state = route(
        Some(backend),
        &context(env.agent_dir.path(), Arc::default()),
        "computer_use.app",
        &json!({"handle": handle, "method": "get_ax_state", "diff": true}),
    )
    .unwrap();
    assert_eq!(state, json!("(no changes since the previous observation)"));
    let frontmost = route(
        Some(backend),
        &context(env.agent_dir.path(), Arc::default()),
        "computer_use.app",
        &json!({"handle": handle, "method": "is_frontmost"}),
    )
    .unwrap();
    assert_eq!(frontmost, json!(false));
}

#[test]
fn every_app_method_decodes() {
    let point = json!({"x": 1, "y": 2.5, "repr": "(1, 2.5)"});
    let calls = [
        json!({"method": "get_ax_state", "diff": false}),
        json!({"method": "get_screenshot"}),
        json!({"method": "get_text_regions"}),
        json!({"method": "click", "target": {"kind": "index", "index": 2}, "button": "right", "count": 2}),
        json!({"method": "click", "target": {"kind": "point", "point": point}, "button": "left", "count": 1}),
        json!({"method": "click", "target": {"kind": "invalid", "type": "str"}, "button": "middle", "count": 10}),
        json!({"method": "drag", "from": point, "to": {"repr": "'x'"}}),
        json!({"method": "scroll", "target": {"kind": "index", "index": 0}, "direction": "down", "pages": 3}),
        json!({"method": "press_key", "key": {"text": "cmd+s"}}),
        json!({"method": "type_text", "text": {"type": "int"}}),
        json!({"method": "set_value", "element_index": {"index": 1}, "value": "v"}),
        json!({"method": "select_text", "element_index": {"type": "str"}, "text": "t", "prefix": null, "suffix": "s"}),
        json!({"method": "perform_secondary_action", "element_index": {"index": 1}, "action": {"name": "AXPress"}}),
        json!({"method": "perform_secondary_action", "element_index": {"index": 1}, "action": {"str": "5"}}),
        json!({"method": "paste", "text": "p", "format": "html"}),
        json!({"method": "activate"}),
        json!({"method": "is_frontmost"}),
    ];
    for payload in calls {
        assert!(decode_call(&payload).is_some(), "{payload}");
    }
    assert_eq!(
        decode_call(&json!({"method": "drag", "from": point, "to": {"repr": "'x'"}})),
        Some(AppCall::Drag {
            from: PointArg::Valid {
                x: 1.0,
                y: 2.5,
                repr: "(1, 2.5)".to_string()
            },
            to: PointArg::Invalid {
                repr: "'x'".to_string()
            },
        })
    );
}

#[test]
fn malformed_calls_are_refused_not_guessed() {
    for payload in [
        json!({"method": "click", "target": {"kind": "index", "index": 0}, "button": "side", "count": 1}),
        json!({"method": "click", "target": {"kind": "index", "index": 0}, "button": "left", "count": 11}),
        json!({"method": "click", "target": {"kind": "index", "index": 0}, "button": "left", "count": 0}),
        json!({"method": "scroll", "target": {"kind": "index", "index": 0}, "direction": "up", "pages": 0}),
        json!({"method": "select_text", "element_index": {"index": 0}, "text": ""}),
        json!({"method": "paste", "text": "p", "format": "rtf"}),
        json!({"method": "launch_missiles"}),
        json!({}),
    ] {
        assert_eq!(decode_call(&payload), None, "{payload}");
    }
    let env = Env::new();
    let error = route(
        Some(&env.session),
        &context(env.agent_dir.path(), Arc::default()),
        "computer_use.app",
        &json!({"handle": 1, "method": "nope"}),
    )
    .unwrap_err();
    assert_eq!(error, invalid("malformed computer_use.app request"));
}

#[test]
fn without_a_backend_state_reads_empty_and_binding_fails_with_transport() {
    let agent_dir = tempfile::tempdir().unwrap();
    let telemetry = Arc::new(RecordingTelemetry::default());
    let context = context(agent_dir.path(), Arc::clone(&telemetry));
    let state = route(
        None,
        &context,
        "computer_use.get_state",
        &json!({"emit": true}),
    )
    .unwrap();
    assert_eq!(
        state,
        json!({
            "apps": [],
            "permissions": {"accessibility": "unknown", "screen_recording": "unknown", "help": MAC_HELP_LINES},
            "allowlist": {"allowed": [], "blocked": [], "system_deny": crate::policy::SYSTEM_DENY, "risk": {}},
            "platform": null,
        })
    );
    assert!(matches!(
        &telemetry.events()[..],
        [
            TelemetryEvent::SessionStarted {
                platform: "unknown"
            },
            TelemetryEvent::Action {
                action: "get_state",
                outcome: Outcome::Ok,
                ..
            }
        ]
    ));
    let bind = route(
        None,
        &context,
        "computer_use.get_app",
        &json!({"spec": spec("Example")}),
    )
    .unwrap_err();
    assert_eq!(bind.code, crate::error::ErrorCode::TransportError);
    assert!(bind.message.contains("computer use backend unavailable"));
    let list = route(None, &context, "computer_use.list_apps", &json!({})).unwrap_err();
    assert_eq!(list.code, crate::error::ErrorCode::TransportError);
    assert_eq!(
        route(
            None,
            &context,
            "computer_use.permissions_status",
            &json!({})
        )
        .unwrap()["accessibility"],
        json!("unknown")
    );
    assert_eq!(
        route(None, &context, "computer_use.nope", &json!({}))
            .unwrap_err()
            .code,
        crate::error::ErrorCode::InvalidArgument
    );
}

#[test]
fn replies_carry_ok_or_the_error_wire_form() {
    let host = ComputerUse::new(HostConfig {
        agent_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
        telemetry: Arc::new(crate::telemetry::NoTelemetry),
    });
    let reply = host.handle_blocking("computer_use.nope", &json!({}));
    assert_eq!(
        reply,
        json!({"error": {"code": "INVALID_ARGUMENT", "message": "unknown computer-use request type \"computer_use.nope\"", "details": null}})
    );
}

/// Compile-checked here; runs on macOS only.
#[cfg(target_os = "macos")]
#[test]
fn darwin_wins_ahead_of_every_linux_backend() {
    let script = Script::with_tools(&["xdotool"]);
    script.set_env("WAYLAND_DISPLAY", "wayland-1");
    script.set_env("NIRI_SOCKET", "/run/niri.sock");
    script.add_socket("/run/niri.sock");
    assert_eq!(detect_kind(&script), Some(PlatformKind::Mac));
}

#[cfg(target_os = "linux")]
#[test]
fn a_niri_session_selects_wayland_ahead_of_x11() {
    let script = Script::with_tools(&["xdotool"]);
    script.set_env("WAYLAND_DISPLAY", "wayland-1");
    script.set_env("NIRI_SOCKET", "/run/niri.sock");
    script.set_env("DISPLAY", ":0");
    script.add_socket("/run/niri.sock");
    assert_eq!(detect_kind(&script), Some(PlatformKind::Wayland));
}

#[cfg(target_os = "linux")]
#[test]
fn without_niri_the_x11_selection_is_unchanged() {
    let script = Script::with_tools(&["xdotool"]);
    script.set_env("WAYLAND_DISPLAY", "wayland-1");
    assert_eq!(detect_kind(&script), Some(PlatformKind::X11));
    let script = Script::with_tools(&["xdotool"]);
    script.set_env("NIRI_SOCKET", "/run/niri.sock");
    script.add_socket("/run/niri.sock");
    assert_eq!(detect_kind(&script), Some(PlatformKind::X11));
    let script = Script::with_tools(&[]);
    script.set_env("WAYLAND_DISPLAY", "wayland-1");
    assert_eq!(detect_kind(&script), None);
}

#[cfg(target_os = "linux")]
#[test]
fn a_stale_or_non_socket_niri_path_is_not_niri() {
    let script = Script::with_tools(&["xdotool"]);
    script.set_env("WAYLAND_DISPLAY", "w");
    script.set_env("NIRI_SOCKET", "/tmp/plain");
    script.add_file("/tmp/plain");
    assert_eq!(detect_kind(&script), Some(PlatformKind::X11));
}

#[cfg(target_os = "linux")]
#[test]
fn a_real_socket_path_reads_as_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("niri.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let tools = crate::process::SystemTools;
    assert!(crate::process::Tools::is_socket(
        &tools,
        path.to_str().unwrap()
    ));
    let plain = dir.path().join("plain");
    std::fs::write(&plain, "x").unwrap();
    assert!(!crate::process::Tools::is_socket(
        &tools,
        plain.to_str().unwrap()
    ));
    assert!(!crate::process::Tools::is_socket(
        &tools,
        "/nonexistent/niri.sock"
    ));
}

/// A backend whose every request blocks until released.
struct Blocking {
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl Backend for Blocking {
    fn get_state(&self, _emit: bool) -> Result<Value> {
        // Bounded: a dispatch on the runtime thread fails the test instead
        // of hanging it.
        match self
            .release
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))
        {
            Ok(()) => Ok(json!("released")),
            Err(_) => Err(transport("never released")),
        }
    }
    fn list_apps(&self) -> Result<Value> {
        Ok(json!([]))
    }
    fn permissions(&self) -> Value {
        json!({})
    }
    fn get_app(&self, _spec: &AppSpec, _instructions_dir: Option<PathBuf>) -> Result<Value> {
        Ok(json!({}))
    }
    fn call(&self, _handle: u64, _call: AppCall) -> Result<Value> {
        Ok(json!(null))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_blocking_backend_call_never_stalls_the_async_runtime() {
    // On a single-threaded runtime, a request that blocked the runtime
    // thread would starve this task's own timer and the release below.
    let (release, gate) = std::sync::mpsc::channel();
    let backend: Arc<dyn Backend> = Arc::new(Blocking {
        release: std::sync::Mutex::new(gate),
    });
    let agent_dir = tempfile::tempdir().unwrap();
    let context = Arc::new(context(agent_dir.path(), Arc::default()));
    let request = {
        let backend = Arc::clone(&backend);
        tokio::spawn(run_blocking(move || {
            route(
                Some(backend.as_ref()),
                &context,
                "computer_use.get_state",
                &json!({}),
            )
        }))
    };
    tokio::task::yield_now().await;
    assert!(!request.is_finished());
    release.send(()).unwrap();
    let reply = request.await.unwrap();
    assert_eq!(reply, json!({"ok": "released"}));
}
