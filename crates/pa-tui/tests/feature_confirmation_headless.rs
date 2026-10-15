//! Headless e2e for the questions a session's hooks and plugins put to the
//! user (the `confirmation` feature status): the oldest pending question
//! opens the yes/no confirm panel with No selected, Enter answers it
//! (`answer_feature_confirmation`), a lagging status that still lists the
//! answered question does not reopen it, the next question opens, and Up
//! then Enter allows it.
#![cfg(unix)]
// Casts: structurally bounded terminal-layout arithmetic; guarded conversions add panic paths.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Render routes are flat tables (one arm per route); splitting adds indirection.
#![allow(clippy::too_many_lines)]
// Widget state structs carry independent flag bits.
#![allow(clippy::struct_excessive_bools, clippy::fn_params_excessive_bools)]
// Futures are bounded by the surface's lifetime; boxing adds a steady-state allocation.
#![allow(clippy::large_futures)]
// The wrappers preserve a uniform Result-returning API surface.
#![allow(clippy::unnecessary_wraps)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{
    HeadlessPlan,
    HeadlessStep,
    InteractiveOptions,
    ModelSelection,
    SessionSelection,
    UiMode,
    run_interactive,
};
use serde_json::{Value, json};

/// The answers the mock daemon received: (confirmation id, allow).
type Answers = Arc<Mutex<Vec<(String, bool)>>>;

struct MockSupervisor {
    listener: UnixListener,
    answers: Answers,
}

/// The session's `confirmation` status with `pending` questions.
fn confirmation_event(pending: &[(&str, &str)]) -> Value {
    let confirmations: Vec<Value> = pending
        .iter()
        .map(|(id, action)| json!({ "id": id, "action": action, "reason": "guard asks first" }))
        .collect();
    json!({
        "type": "session_event",
        "activeSessionId": "s1",
        "event": {
            "type": "feature_status",
            "feature": "confirmation",
            "line": null,
            "status": { "confirmations": confirmations },
        },
    })
}

impl MockSupervisor {
    /// Serve one connection: after the attach, one question is pending; its
    /// answer is followed by a lagging status still listing it beside the
    /// next one, then the next one alone.
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        let mut writer = stream.try_clone().expect("clone mock socket");
        let mut reader = BufReader::new(stream);
        write_json(
            &mut writer,
            &json!({
                "type": "daemon_hello",
                "protocol": { "name": "prime-agent.daemon", "version": 7 },
                "serverCapabilities": ["feature_confirmation"],
                "clientId": "mock",
            }),
        );
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(envelope) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let id = envelope.get("id").and_then(Value::as_str).unwrap_or("");
            let command = envelope.get("command").cloned().unwrap_or(Value::Null);
            let command_type = command
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let response = |data: Value| {
                json!({
                    "type": "response",
                    "id": id,
                    "command": command_type,
                    "success": true,
                    "data": data,
                })
            };
            match command_type.as_str() {
                "create" => write_json(
                    &mut writer,
                    &response(json!({
                        "activeSessionId": "s1",
                        "id": "s1",
                        "sessionId": "sess-1",
                        "sessionFile": "/tmp/sess-1.jsonl",
                    })),
                ),
                "attach" => {
                    write_json(&mut writer, &attach_data(id));
                    write_json(
                        &mut writer,
                        &confirmation_event(&[("confirm-1", "Run `git push origin main`")]),
                    );
                }
                "answer_feature_confirmation" => {
                    let confirmation = command["confirmationId"].as_str().unwrap_or_default();
                    let allow = command["allow"].as_bool().expect("allow is a bool");
                    self.answers
                        .lock()
                        .unwrap()
                        .push((confirmation.to_string(), allow));
                    write_json(&mut writer, &response(json!({ "answered": true })));
                    if confirmation == "confirm-1" {
                        write_json(
                            &mut writer,
                            &confirmation_event(&[
                                ("confirm-1", "Run `git push origin main`"),
                                ("confirm-2", "Run `sudo make install`"),
                            ]),
                        );
                        write_json(
                            &mut writer,
                            &confirmation_event(&[("confirm-2", "Run `sudo make install`")]),
                        );
                    } else {
                        write_json(&mut writer, &confirmation_event(&[]));
                    }
                }
                _ => write_json(&mut writer, &response(json!({}))),
            }
        }
    }
}

fn write_json(writer: &mut UnixStream, value: &Value) {
    let mut line = serde_json::to_string(value).expect("serialize mock frame");
    line.push('\n');
    writer.write_all(line.as_bytes()).expect("write mock frame");
    writer.flush().expect("flush mock frame");
}

/// The slim attach result: one empty session.
fn attach_data(id: &str) -> Value {
    json!({
        "type": "response",
        "id": id,
        "command": "attach",
        "success": true,
        "data": {
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "activeSessionId": "s1",
            "snapshot": {
                "activeSessionId": "s1",
                "summary": { "id": "s1", "cwd": "/tmp" },
                "state": {
                    "activeSessionId": "s1",
                    "cwd": "/tmp",
                    "sessionId": "sess-1",
                    "sessionName": "confirm session",
                    "model": null,
                    "isStreaming": false,
                    "isCompacting": false,
                    "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                },
                "messages": [],
                "lastEventSequence": 0,
                "lastEventCursor": null,
            },
            "client": { "id": "mock", "capabilities": [] },
            "lastEventSequence": 0,
            "lastEventCursor": null,
        },
    })
}

fn options(socket: PathBuf) -> InteractiveOptions {
    InteractiveOptions {
        sandbox_mode: None,
        resource_exclusions: pa_types::daemon::SessionResourceExclusions::default(),
        initial_plan_mode: false,
        models: None,
        socket_path: socket,
        cwd: PathBuf::from("/tmp"),
        session_dir: None,
        script_path: None,
        model_selection: ModelSelection::default(),
        model_catalog: Vec::new(),
        model_configured_providers: std::collections::HashSet::default(),
        model_recent_models: Vec::new(),
        default_thinking_level: None,
        no_session: false,
        session: SessionSelection::New,
        initial_message: None,
        show_images: true,
        fullscreen_mouse: true,
        theme: "prime".to_string(),
        code_block_indent: "  ".to_string(),
        tree_filter_mode: String::new(),
        branch_summary_skip_prompt: false,
        version: "0.0.0".to_string(),
        onboarding: None,
        telemetry_disabled: None,
        client_auth: None,
        traces: None,
        provider_auth: None,
        update_commands: None,
        telemetry: None,
        keybindings: pa_tui::keybindings::KeybindingsManager::new(),
        session_rlm_depth: None,
        prompt_stash: std::sync::Arc::default(),
        session_has_children: false,
        restore_dock_focus: false,
        client_settings: None,
    }
}

fn key(code: KeyCode) -> HeadlessStep {
    HeadlessStep::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn hook_questions_open_the_confirm_panel_and_send_the_answers() {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let answers: Answers = Arc::default();
    let supervisor = MockSupervisor {
        listener: UnixListener::bind(&socket).expect("bind mock socket"),
        answers: Arc::clone(&answers),
    };
    let handle = std::thread::spawn(move || supervisor.serve());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps: vec![
            HeadlessStep::WaitRender {
                needle: "Run `git push origin main`".to_string(),
                timeout_ms: 5_000,
            },
            // No is selected: Enter refuses.
            key(KeyCode::Enter),
            HeadlessStep::WaitRender {
                needle: "Run `sudo make install`".to_string(),
                timeout_ms: 5_000,
            },
            key(KeyCode::Up),
            key(KeyCode::Enter),
            HeadlessStep::WaitGone {
                needle: "Allow this action?".to_string(),
                timeout_ms: 5_000,
            },
        ],
        width: 100,
        height: 30,
    };
    let outcome = runtime
        .block_on(run_interactive(options(socket), UiMode::Headless(plan)))
        .expect("interactive run");
    handle.join().expect("mock supervisor finished");
    let asked = outcome
        .frames
        .iter()
        .find(|frame| frame.contains("Run `git push origin main`"))
        .expect("the first question rendered");
    assert!(asked.contains("Allow this action?"), "{asked}");
    assert!(asked.contains("guard asks first"), "{asked}");
    assert_eq!(
        *answers.lock().unwrap(),
        vec![
            ("confirm-1".to_string(), false),
            ("confirm-2".to_string(), true)
        ]
    );
}
