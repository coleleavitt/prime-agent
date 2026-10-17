//! Headless e2e for the session modes a session publishes: in brief mode
//! (`brief` status on) the `rlm.send_user_message` row renders as a
//! `● Message` while its turn's plain assistant text collapses, and a
//! coordinator session (`coordinator` status) refuses a `/fork` that keeps
//! the original running.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

fn status_event(feature: &str, status: &Value) -> Value {
    json!({
        "type": "session_event",
        "activeSessionId": "s1",
        "event": { "type": "feature_status", "feature": feature, "line": null, "status": status },
    })
}

fn write_json(writer: &mut UnixStream, value: &Value) {
    let mut line = serde_json::to_string(value).expect("serialize mock frame");
    line.push('\n');
    writer.write_all(line.as_bytes()).expect("write mock frame");
    writer.flush().expect("flush mock frame");
}

fn transcript() -> Vec<Value> {
    vec![
        json!({ "role": "user", "content": "how is the build?", "timestamp": 1_000u64 }),
        json!({
            "role": "assistant",
            "stopReason": "stop",
            "content": [{ "type": "text", "text": "plain narration alpha" }],
            "timestamp": 2_000u64,
        }),
        json!({
            "role": "custom",
            "customType": "user_message",
            "display": true,
            "content": "the real answer beta",
            "details": {
                "status": "normal",
                "attachments": [{ "path": "/tmp/build.log", "size": 12, "isImage": false }],
                "sentAt": 3_000u64,
            },
            "timestamp": 3_000u64,
        }),
    ]
}

/// The attach result: one session with `messages`.
fn attach_data(id: &str, messages: &[Value]) -> Value {
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
                    "sessionName": "modes session",
                    "model": null,
                    "isStreaming": false,
                    "isCompacting": false,
                    "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                },
                "messages": messages,
                "lastEventSequence": 0,
                "lastEventCursor": null,
            },
            "client": { "id": "mock", "capabilities": [] },
            "lastEventSequence": 0,
            "lastEventCursor": null,
        },
    })
}

/// Serve one client: attach with the transcript, then publish `statuses`.
fn serve(listener: &UnixListener, statuses: &[(&str, Value)], commands: &Arc<Mutex<Vec<String>>>) {
    let (stream, _) = listener.accept().expect("accept");
    let mut writer = stream.try_clone().expect("clone mock socket");
    let mut reader = BufReader::new(stream);
    write_json(
        &mut writer,
        &json!({
            "type": "daemon_hello",
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "serverCapabilities": [],
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
        commands.lock().unwrap().push(command_type.clone());
        match command_type.as_str() {
            "create" => write_json(
                &mut writer,
                &json!({
                    "type": "response",
                    "id": id,
                    "command": "create",
                    "success": true,
                    "data": {
                        "activeSessionId": "s1",
                        "id": "s1",
                        "sessionId": "sess-1",
                        "sessionFile": "/tmp/sess-1.jsonl",
                    },
                }),
            ),
            "attach" => {
                write_json(&mut writer, &attach_data(id, &transcript()));
                for (feature, status) in statuses {
                    write_json(&mut writer, &status_event(feature, status));
                }
            }
            _ => write_json(
                &mut writer,
                &json!({
                    "type": "response",
                    "id": id,
                    "command": command_type,
                    "success": true,
                    "data": {},
                }),
            ),
        }
    }
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

fn wait(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 5_000,
    }
}

fn gone(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitGone {
        needle: needle.to_string(),
        timeout_ms: 5_000,
    }
}

/// What one run left: the captured frames and the commands the client sent.
struct Run {
    frames: Vec<String>,
    commands: Vec<String>,
}

/// Run the TUI against a mock daemon publishing `statuses`.
fn run(statuses: Vec<(&'static str, Value)>, steps: Vec<HeadlessStep>) -> Run {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let listener = UnixListener::bind(&socket).expect("bind mock socket");
    let commands: Arc<Mutex<Vec<String>>> = Arc::default();
    let handle = {
        let commands = Arc::clone(&commands);
        std::thread::spawn(move || serve(&listener, &statuses, &commands))
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let outcome = runtime
        .block_on(run_interactive(
            options(socket),
            UiMode::Headless(HeadlessPlan {
                steps,
                width: 100,
                height: 30,
            }),
        ))
        .expect("interactive run");
    handle.join().expect("mock supervisor finished");
    let commands = commands.lock().unwrap().clone();
    Run {
        frames: outcome.frames,
        commands,
    }
}

fn last_frame(run: &Run) -> &str {
    run.frames.last().expect("a frame")
}

#[test]
fn brief_mode_shows_the_message_and_collapses_the_turns_text() {
    // Brief mode off: the message renders and the text stays.
    let off = run(
        Vec::new(),
        vec![wait("\u{25cf} Message"), wait("plain narration alpha")],
    );
    let frame = last_frame(&off);
    for needle in [
        "\u{25cf} Message",
        "the real answer beta",
        "\u{203a} [file] /tmp/build.log (12 B)",
        "plain narration alpha",
    ] {
        assert!(frame.contains(needle), "{needle:?} in {frame}");
    }
    // Brief mode on: the turn's plain text collapses; the message stays.
    let on = run(
        vec![("brief", json!({ "enabled": true }))],
        vec![wait("the real answer beta"), gone("plain narration alpha")],
    );
    let frame = last_frame(&on);
    assert!(frame.contains("the real answer beta"), "{frame}");
    assert!(!frame.contains("plain narration alpha"), "{frame}");
}

#[test]
fn a_coordinator_session_refuses_a_fork_that_keeps_the_original() {
    // The brief status lands after the coordinator one: its collapse is the
    // observable sign both were applied before the command.
    let refused = run(
        vec![
            ("coordinator", json!({ "enabled": true })),
            ("brief", json!({ "enabled": true })),
        ],
        vec![
            wait("the real answer beta"),
            gone("plain narration alpha"),
            HeadlessStep::Submit("/fork".to_string()),
            wait("Forking is not available in coordinator sessions."),
        ],
    );
    assert!(
        refused
            .frames
            .iter()
            .any(|frame| frame.contains("Forking is not available in coordinator sessions.")),
        "the refusal renders: {}",
        last_frame(&refused)
    );
    assert!(
        !refused
            .commands
            .iter()
            .any(|command| command == "get_user_messages_for_forking"),
        "the fork selector never opened: {:?}",
        refused.commands
    );
}
