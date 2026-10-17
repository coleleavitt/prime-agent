//! Headless e2e for the permission-mode surfaces a feature publishes: a
//! question offering to remember its answer gets a third choice ("Yes, and
//! don't ask again for <rule>") answered with `remember: true`; the
//! `permissions` status's indicator renders under the editor; and the plan
//! key (shift+tab) sends the status's `cycle` commands instead of `/plan`.
#![cfg(unix)]
// The mock's route table is one flat match.
#![allow(clippy::too_many_lines)]
// Futures are bounded by the surface's lifetime; boxing adds a steady-state allocation.
#![allow(clippy::large_futures)]

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

/// What the mock daemon received: answers `(id, allow, remember)` and
/// prompt messages.
#[derive(Default)]
struct Received {
    answers: Vec<(String, bool, bool)>,
    prompts: Vec<String>,
}

fn status_event(feature: &str, status: &Value) -> Value {
    json!({
        "type": "session_event",
        "activeSessionId": "s1",
        "event": { "type": "feature_status", "feature": feature, "line": null, "status": status },
    })
}

fn permissions(mode: &str, panel: &[&str], cycle: &[&str]) -> Value {
    status_event(
        "permissions",
        &json!({ "mode": mode, "panel": panel, "cycle": cycle }),
    )
}

fn serve(listener: &UnixListener, received: &Arc<Mutex<Received>>) {
    let (stream, _) = listener.accept().expect("accept");
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
                    &permissions("default", &[], &["/permissions mode acceptEdits"]),
                );
                write_json(
                    &mut writer,
                    &status_event(
                        "confirmation",
                        &json!({ "confirmations": [{
                            "id": "confirm-1",
                            "action": "Run `npm test`",
                            "reason": "Prime Agent requested permissions to use Bash",
                            "remember": "Bash(npm test *)",
                        }] }),
                    ),
                );
            }
            "answer_feature_confirmation" => {
                received.lock().unwrap().answers.push((
                    command["confirmationId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    command["allow"].as_bool().expect("allow is a bool"),
                    command["remember"].as_bool().unwrap_or(false),
                ));
                write_json(&mut writer, &response(json!({ "answered": true })));
                write_json(
                    &mut writer,
                    &status_event("confirmation", &json!({ "confirmations": [] })),
                );
            }
            "prompt" => {
                let message = command["message"].as_str().unwrap_or_default().to_string();
                write_json(&mut writer, &response(json!({})));
                match message.as_str() {
                    "/permissions mode acceptEdits" => write_json(
                        &mut writer,
                        &permissions(
                            "acceptEdits",
                            &["\u{25b8}\u{25b8} accept edits on (shift+tab to cycle)"],
                            &["/permissions mode plan", "/plan on"],
                        ),
                    ),
                    "/plan on" => write_json(
                        &mut writer,
                        &permissions(
                            "plan",
                            &["\u{2016} plan mode on (shift+tab to cycle)"],
                            &["/plan off", "/permissions mode default"],
                        ),
                    ),
                    _ => {}
                }
                // A session command's turn ends at once.
                for event in ["turn_end", "agent_end"] {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "session_event",
                            "activeSessionId": "s1",
                            "event": { "type": event },
                        }),
                    );
                }
                received.lock().unwrap().prompts.push(message);
            }
            _ => write_json(&mut writer, &response(json!({}))),
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
                    "sessionName": "permissions session",
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

fn wait(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 5_000,
    }
}

#[test]
fn remembered_answers_and_the_mode_cycle_reach_the_daemon() {
    std::env::remove_var("TMUX");
    // The feature's command, as `pa_core::features::install` registers it.
    pa_types::slash_commands::register_feature_slash_commands(vec![
        pa_types::slash_commands::BuiltinSlashCommand {
            name: "permissions",
            description: "permission rules and modes",
            execution: pa_types::slash_commands::SlashCommandExecution::Session,
            argument_hint: None,
            aliases: &[],
            takes_argument: true,
        },
    ]);
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let received: Arc<Mutex<Received>> = Arc::default();
    let listener = UnixListener::bind(&socket).expect("bind mock socket");
    let handle = {
        let received = Arc::clone(&received);
        std::thread::spawn(move || serve(&listener, &received))
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps: vec![
            wait("don't ask again for Bash(npm test *)"),
            // No is selected; one up is the remember choice.
            key(KeyCode::Up),
            key(KeyCode::Enter),
            HeadlessStep::WaitGone {
                needle: "Allow this action?".to_string(),
                timeout_ms: 5_000,
            },
            key(KeyCode::BackTab),
            wait("accept edits on (shift+tab to cycle)"),
            key(KeyCode::BackTab),
            wait("plan mode on (shift+tab to cycle)"),
        ],
        width: 100,
        height: 30,
    };
    runtime
        .block_on(run_interactive(options(socket), UiMode::Headless(plan)))
        .expect("interactive run");
    handle.join().expect("mock supervisor finished");
    let received = received.lock().unwrap();
    assert_eq!(
        received.answers,
        vec![("confirm-1".to_string(), true, true)]
    );
    assert_eq!(
        received.prompts,
        vec![
            "/permissions mode acceptEdits".to_string(),
            "/permissions mode plan".to_string(),
            "/plan on".to_string(),
        ]
    );
}
