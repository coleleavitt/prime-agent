//! Headless e2e for the working loader across a model-turn boundary. The
//! agent loop emits `turn_end` after every model call's tool results and
//! `turn_start` before the next call; the loader rows (a spacer and the
//! spinner line under the transcript) must stay up across that boundary,
//! or the bottom-anchored transcript drops two rows when the cell's
//! marker turns into a checkmark and jumps back when the next call starts.
//! The run also stays busy across it: a Ctrl+C there interrupts the run
//! instead of arming the exit.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

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

/// The gap the mock leaves between one model call's `turn_end` and the
/// next call's `turn_start`: long enough for the client to paint a frame.
const TURN_GAP_MS: u64 = 400;

/// Serve one client connection; returns the command types it sent.
fn serve(listener: &UnixListener) -> Vec<String> {
    listener
        .set_nonblocking(true)
        .expect("nonblocking mock listener");
    let idle_until = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    let (stream, _) = loop {
        match listener.accept() {
            Ok(accepted) => break accepted,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= idle_until {
                    return Vec::new();
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return Vec::new(),
        }
    };
    stream.set_nonblocking(false).expect("blocking mock stream");
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
    let mut commands = Vec::new();
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
        let command_type = envelope
            .pointer("/command/type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        commands.push(command_type.clone());
        if command_type == "attach" {
            write_json(&mut writer, &attach_data(id));
            let mut w = writer.try_clone().expect("clone event socket");
            std::thread::spawn(move || two_model_calls(&mut w));
            continue;
        }
        let data = if command_type == "create" {
            json!({
                "activeSessionId": "s1",
                "id": "s1",
                "sessionId": "sess-1",
                "sessionFile": "/tmp/sess-1.jsonl",
            })
        } else {
            json!({})
        };
        write_json(
            &mut writer,
            &json!({
                "type": "response",
                "id": id,
                "command": command_type,
                "success": true,
                "data": data,
            }),
        );
    }
    commands
}

/// One cell runs and settles, its model call ends, and after a gap the
/// next model call starts and streams its answer.
fn two_model_calls(w: &mut UnixStream) {
    let call = json!({
        "type": "toolCall",
        "id": "call_1",
        "name": "python_repl",
        "arguments": { "code": "print(1)" },
    });
    event(
        w,
        &json!({
            "type": "tool_execution_start",
            "toolCallId": "call_1",
            "toolName": "python_repl",
            "args": { "code": "print(1)" },
        }),
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    let result = json!({ "content": [{ "type": "text", "text": "1" }], "isError": false });
    event(
        w,
        &json!({
            "type": "tool_execution_end",
            "toolCallId": "call_1",
            "toolName": "python_repl",
            "result": result,
        }),
    );
    event(
        w,
        &json!({
            "type": "turn_end",
            "message": { "role": "assistant", "content": [call], "stopReason": "toolUse" },
            "toolResults": [{
                "role": "toolResult",
                "toolCallId": "call_1",
                "toolName": "python_repl",
                "content": [{ "type": "text", "text": "1" }],
                "isError": false,
            }],
        }),
    );
    std::thread::sleep(std::time::Duration::from_millis(TURN_GAP_MS));
    event(w, &json!({ "type": "turn_start" }));
    drain_barrier(w);
    std::thread::sleep(std::time::Duration::from_millis(TURN_GAP_MS));
    event(
        w,
        &json!({
            "type": "turn_end",
            "message": { "role": "assistant", "content": [], "stopReason": "stop" },
            "toolResults": [],
        }),
    );
    event(w, &json!({ "type": "agent_end", "messages": [] }));
}

fn write_json(writer: &mut UnixStream, value: &Value) {
    let mut line = serde_json::to_string(value).expect("serialize mock frame");
    line.push('\n');
    writer.write_all(line.as_bytes()).expect("write mock frame");
    writer.flush().expect("flush mock frame");
}

/// One session event frame on the mock socket.
fn event(writer: &mut UnixStream, payload: &Value) {
    write_json(
        writer,
        &json!({
            "type": "session_event",
            "activeSessionId": "s1",
            "event": payload,
        }),
    );
}

/// The drain barrier: a visible user row whose render is the observable
/// proof the frames written just before it on the same socket were
/// processed first, so the tests' negative assertions cannot pass
/// vacuously when the plan finishes early.
fn drain_barrier(writer: &mut UnixStream) {
    event(
        writer,
        &json!({
            "type": "message_start",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "late-probe-drained" }],
            },
        }),
    );
}

/// The attach result: a streaming mid-turn snapshot whose prompt landed
/// just now, so the working loader (and the turn) is active when the
/// tool call starts.
fn attach_data(id: &str) -> Value {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_millis();
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
                    "sessionName": "live timer session",
                    "model": null,
                    "isStreaming": true,
                    "isCompacting": false,
                    "sessionActions": { "queuedCount": 0, "steering": [], "followUps": [] },
                },
                "messages": [
                    {
                        "role": "user",
                        "content": [{ "type": "text", "text": "run the sweep" }],
                        "timestamp": now_ms,
                    },
                ],
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
        fullscreen_mouse: false,
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

/// Run one headless plan against the mock: the frames and the commands
/// the client sent.
fn run(steps: Vec<HeadlessStep>) -> (Vec<String>, Vec<String>) {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let listener = UnixListener::bind(&socket).expect("bind mock socket");
    let handle = std::thread::spawn(move || serve(&listener));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps,
        width: 100,
        height: 40,
    };
    let outcome = runtime
        .block_on(run_interactive(options(socket), UiMode::Headless(plan)))
        .expect("interactive run");
    let commands = handle.join().expect("mock supervisor finished");
    (outcome.frames, commands)
}

fn drained() -> HeadlessStep {
    HeadlessStep::WaitRender {
        needle: "late-probe-drained".to_string(),
        timeout_ms: 5000,
    }
}

#[test]
fn the_loader_stays_up_between_model_calls() {
    let (frames, _) = run(vec![drained()]);
    let settled = frames
        .iter()
        .position(|frame| frame.contains("\u{2713} python"))
        .expect("the settled cell renders its checkmark");
    let barrier = frames
        .iter()
        .position(|frame| frame.contains("late-probe-drained"))
        .expect("the drain barrier rendered");
    let window = &frames[settled..=barrier];
    assert!(
        window.iter().all(|frame| frame.contains("Waiting")),
        "the loader dropped between model calls:\n{}",
        window.join("\n===\n")
    );
}

#[test]
fn ctrl_c_between_model_calls_interrupts_the_run() {
    let (frames, commands) = run(vec![
        HeadlessStep::WaitRender {
            needle: "\u{2713} python".to_string(),
            timeout_ms: 5000,
        },
        HeadlessStep::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        )),
        drained(),
    ]);
    assert!(
        commands.iter().any(|command| command == "abort"),
        "a Ctrl+C between model calls sends no abort (commands {commands:?}):\n{}",
        frames.join("\n===\n")
    );
}
