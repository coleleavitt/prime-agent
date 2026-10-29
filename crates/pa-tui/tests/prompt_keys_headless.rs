//! Headless e2e for Claude Code's prompt keys: Ctrl+C clears an idle draft
//! into a slot Up restores and arms the exit, a second press exits; a press
//! with a turn running only interrupts (never arms the exit); Up on an empty
//! editor takes the queued messages out of the queue into the editor;
//! Ctrl+Enter sends the draft now (steer, then interrupt); Ctrl+Alt+X clears
//! the queue.
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

/// Every command the mock daemon received, in order.
type Commands = Arc<Mutex<Vec<Value>>>;

struct MockSupervisor {
    listener: UnixListener,
    commands: Commands,
    streaming: bool,
    steering: Vec<&'static str>,
    /// The abort after which the running turn ends (`agent_end`).
    aborts_before_end: usize,
    /// Advertise `cell_detach` and answer `detach_running_cell` with `detach`.
    cell_detach: bool,
    detach: DetachAnswer,
}

/// How the mock answers `detach_running_cell`.
#[derive(Clone, Copy)]
enum DetachAnswer {
    /// The cell moved to the background; the run takes the queue and ends.
    Moved,
    /// A cell blocks the loop and cannot move; the step runs to its end.
    Blocked,
    /// Nothing is running but the reply being written.
    NoCell,
    /// The request fails; the run goes on and ends on its own.
    Fails,
}

impl MockSupervisor {
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        let mut writer = stream.try_clone().expect("clone mock socket");
        let mut reader = BufReader::new(stream);
        write_json(
            &mut writer,
            &json!({
                "type": "daemon_hello",
                "protocol": { "name": "prime-agent.daemon", "version": 7 },
                "serverCapabilities": if self.cell_detach {
                    json!(["abort_and_send_queued", "queue_message_mutation", "cell_detach"])
                } else {
                    json!(["abort_and_send_queued", "queue_message_mutation"])
                },
                "clientId": "mock",
            }),
        );
        let mut queue: Vec<String> = self
            .steering
            .iter()
            .map(|text| (*text).to_string())
            .collect();
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
            self.commands.lock().unwrap().push(command.clone());
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
                "attach" => write_json(
                    &mut writer,
                    &attach_data(id, self.streaming, &self.steering),
                ),
                "abort_and_send_queued" => {
                    write_json(&mut writer, &response(json!({})));
                    let aborts = self
                        .commands
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|command| command["type"] == "abort_and_send_queued")
                        .count();
                    if aborts == self.aborts_before_end {
                        // The turn ends; the steering lane went out with it.
                        queue.clear();
                        write_json(&mut writer, &queue_update(&queue));
                        write_json(&mut writer, &session_event(&json!({ "type": "turn_end" })));
                        write_json(&mut writer, &session_event(&json!({ "type": "agent_end" })));
                    }
                }
                "mutate_queued_message" => {
                    let index = command["index"].as_u64().unwrap_or(0) as usize;
                    let applied =
                        queue.get(index).map(String::as_str) == command["expectedText"].as_str();
                    if applied {
                        queue.remove(index);
                    }
                    write_json(
                        &mut writer,
                        &response(
                            json!({ "status": if applied { "applied" } else { "rejected" } }),
                        ),
                    );
                    write_json(&mut writer, &queue_update(&queue));
                }
                "prompt" if command["streamingBehavior"] == "steer" => {
                    queue.push(command["message"].as_str().unwrap_or_default().to_string());
                    write_json(&mut writer, &response(json!({})));
                    write_json(&mut writer, &queue_update(&queue));
                }
                "detach_running_cell" if matches!(self.detach, DetachAnswer::NoCell) => {
                    write_json(
                        &mut writer,
                        &response(json!({ "detached": false, "reason": "no_cell" })),
                    );
                }
                "detach_running_cell" if matches!(self.detach, DetachAnswer::Fails) => {
                    write_json(
                        &mut writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": command_type,
                            "success": false,
                            "error": "the kernel did not answer",
                        }),
                    );
                    queue.clear();
                    write_json(&mut writer, &queue_update(&queue));
                    write_json(&mut writer, &session_event(&json!({ "type": "turn_end" })));
                    write_json(&mut writer, &session_event(&json!({ "type": "agent_end" })));
                }
                "detach_running_cell" if matches!(self.detach, DetachAnswer::Blocked) => {
                    // A running cell that blocks the loop: it cannot be moved,
                    // and send-now must not interrupt it. The step runs to its
                    // own completion and then the turn takes the queue and ends
                    // -- no abort is involved.
                    write_json(
                        &mut writer,
                        &response(json!({ "detached": false, "reason": "blocked" })),
                    );
                    queue.clear();
                    write_json(&mut writer, &queue_update(&queue));
                    write_json(&mut writer, &session_event(&json!({ "type": "turn_end" })));
                    write_json(&mut writer, &session_event(&json!({ "type": "agent_end" })));
                }
                "detach_running_cell" => {
                    write_json(&mut writer, &response(json!({ "detached": true })));
                    // The run goes on: it takes the queue at its next boundary
                    // and ends on its own.
                    queue.clear();
                    write_json(&mut writer, &queue_update(&queue));
                    write_json(&mut writer, &session_event(&json!({ "type": "turn_end" })));
                    write_json(&mut writer, &session_event(&json!({ "type": "agent_end" })));
                }
                "acquire_session_input_pause" => {
                    write_json(&mut writer, &response(json!({ "pauseId": "pause-1" })));
                }
                "clear_queue" => {
                    queue.clear();
                    write_json(&mut writer, &response(json!({})));
                    write_json(&mut writer, &queue_update(&queue));
                }
                _ => write_json(&mut writer, &response(json!({}))),
            }
        }
    }
}

fn session_event(event: &Value) -> Value {
    json!({ "type": "session_event", "activeSessionId": "s1", "event": event })
}

/// The queue projection after a change.
fn queue_update(queue: &[String]) -> Value {
    session_event(&json!({
        "type": "session_action_update",
        "actions": { "queuedCount": queue.len(), "steering": queue, "followUps": [] },
    }))
}

fn write_json(writer: &mut UnixStream, value: &Value) {
    let mut line = serde_json::to_string(value).expect("serialize mock frame");
    line.push('\n');
    writer.write_all(line.as_bytes()).expect("write mock frame");
    writer.flush().expect("flush mock frame");
}

/// The slim attach result: one empty session.
/// The attached session's transcript: forty user prompts, one of them the
/// search target near the top.
fn transcript() -> Value {
    Value::Array(
        (0..40)
            .map(|n| {
                let text = if n == 2 {
                    "the needle is here".to_string()
                } else {
                    format!("filler prompt number {n}")
                };
                json!({ "role": "user", "content": text, "timestamp": 1_700_000_000_000_u64 + n })
            })
            .collect(),
    )
}

fn attach_data(id: &str, streaming: bool, steering: &[&str]) -> Value {
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
                    "sessionName": "keys session",
                    "model": null,
                    "isStreaming": streaming,
                    "isCompacting": false,
                    "sessionActions": { "queuedCount": steering.len(), "steering": steering, "followUps": [] },
                },
                "messages": transcript(),
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

fn key(code: KeyCode, modifiers: KeyModifiers) -> HeadlessStep {
    HeadlessStep::Key(KeyEvent::new(code, modifiers))
}

fn ctrl(c: char) -> HeadlessStep {
    key(KeyCode::Char(c), KeyModifiers::CONTROL)
}

/// Run `steps` against a session that is `streaming` with `steering`
/// queued; the frames and the commands the daemon received.
fn run(
    streaming: bool,
    steering: Vec<&'static str>,
    aborts_before_end: usize,
    steps: Vec<HeadlessStep>,
) -> (Vec<String>, Vec<Value>, Vec<String>) {
    run_with(streaming, steering, aborts_before_end, false, steps)
}

fn run_with(
    streaming: bool,
    steering: Vec<&'static str>,
    aborts_before_end: usize,
    cell_detach: bool,
    steps: Vec<HeadlessStep>,
) -> (Vec<String>, Vec<Value>, Vec<String>) {
    run_detach(
        streaming,
        steering,
        aborts_before_end,
        cell_detach,
        DetachAnswer::Moved,
        steps,
    )
}

fn run_detach(
    streaming: bool,
    steering: Vec<&'static str>,
    aborts_before_end: usize,
    cell_detach: bool,
    detach: DetachAnswer,
    steps: Vec<HeadlessStep>,
) -> (Vec<String>, Vec<Value>, Vec<String>) {
    std::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let commands: Commands = Arc::default();
    let supervisor = MockSupervisor {
        listener: UnixListener::bind(&socket).expect("bind mock socket"),
        commands: Arc::clone(&commands),
        streaming,
        steering,
        aborts_before_end,
        cell_detach,
        detach,
    };
    let handle = std::thread::spawn(move || supervisor.serve());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let plan = HeadlessPlan {
        steps,
        width: 100,
        height: 30,
    };
    let outcome = runtime
        .block_on(run_interactive(options(socket), UiMode::Headless(plan)))
        .expect("interactive run");
    handle.join().expect("mock supervisor finished");
    let commands = commands.lock().unwrap().clone();
    (outcome.frames, commands, outcome.copies)
}

/// The action commands the TUI sent (its background reads left out), by
/// type: assertion messages never print the commands themselves (the
/// `create` carries the launch environment).
fn types(commands: &[Value]) -> Vec<String> {
    commands
        .iter()
        .filter_map(|command| command["type"].as_str())
        .filter(|kind| {
            kind.starts_with("abort")
                || matches!(
                    *kind,
                    "prompt"
                        | "steer"
                        | "follow_up"
                        | "mutate_queued_message"
                        | "clear_queue"
                        | "detach_running_cell"
                )
        })
        .map(str::to_string)
        .collect()
}

fn wait_for(needle: &str) -> HeadlessStep {
    HeadlessStep::WaitRender {
        needle: needle.to_string(),
        timeout_ms: 5_000,
    }
}

#[test]
fn an_idle_ctrl_c_clears_the_draft_up_restores_it_and_a_second_press_exits() {
    let (frames, commands, _copies) = run(
        false,
        Vec::new(),
        0,
        vec![
            HeadlessStep::Type("draft to keep".to_string()),
            wait_for("draft to keep"),
            ctrl('c'),
            wait_for("Press Ctrl+C again to exit"),
            key(KeyCode::Up, KeyModifiers::NONE),
            wait_for("draft to keep"),
            ctrl('c'),
            ctrl('c'),
            HeadlessStep::WaitMs(100),
        ],
    );
    let hinted = frames
        .iter()
        .find(|frame| frame.contains("Press Ctrl+C again to exit"))
        .expect("the exit hint rendered");
    assert!(
        !hinted.contains("draft to keep"),
        "the press cleared the editor:\n{hinted}"
    );
    assert!(
        frames
            .iter()
            .rev()
            .any(|frame| frame.contains("draft to keep")),
        "Up restored the draft"
    );
    // Nothing reached the daemon: an idle press neither aborts nor submits.
    assert!(types(&commands).is_empty(), "{:?}", types(&commands));
}

#[test]
fn a_ctrl_c_with_a_turn_running_only_interrupts() {
    let (frames, commands, _copies) = run(
        true,
        Vec::new(),
        2,
        vec![
            HeadlessStep::WaitMs(200),
            ctrl('c'),
            HeadlessStep::WaitMs(100),
            ctrl('c'),
            HeadlessStep::WaitMs(200),
        ],
    );
    assert!(
        !frames.iter().any(|frame| frame.contains("again to exit")),
        "an interrupting press never arms the exit"
    );
    assert_eq!(
        types(&commands),
        vec!["abort_and_send_queued", "abort_and_send_queued"],
        "both presses interrupted; neither exited"
    );
}

#[test]
fn up_on_an_empty_editor_takes_the_queued_messages_into_it() {
    let (frames, commands, _copies) = run(
        true,
        vec!["first queued", "second queued"],
        1,
        vec![
            wait_for("second queued"),
            key(KeyCode::Up, KeyModifiers::NONE),
            HeadlessStep::WaitMs(300),
            // The turn ends (the run's settle waits for it).
            ctrl('c'),
            HeadlessStep::WaitMs(100),
        ],
    );
    let deletes: Vec<(u64, &str)> = commands
        .iter()
        .filter(|command| command["type"] == "mutate_queued_message")
        .map(|command| {
            assert_eq!(command["mutation"], json!({ "type": "delete" }));
            (
                command["index"].as_u64().unwrap(),
                command["expectedText"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(deletes, vec![(1, "second queued"), (0, "first queued")]);
    let pulled = frames
        .iter()
        .rev()
        .find(|frame| frame.contains("first queued") && frame.contains("second queued"))
        .expect("the editor shows both pulled messages");
    assert!(
        !pulled.contains("Steering:"),
        "they left the queue strip:\n{pulled}"
    );
}

/// Alt+Up edits a parked message in place, so the browse holds the
/// session's queued input (an input pause) until it ends: a turn boundary
/// meanwhile can neither fold the message into the run nor stop for it.
#[test]
fn browsing_the_queue_holds_it_until_the_browse_ends() {
    let (_frames, commands, _copies) = run(
        true,
        vec!["only queued"],
        1,
        vec![
            wait_for("only queued"),
            key(KeyCode::Up, KeyModifiers::ALT),
            HeadlessStep::WaitMs(200),
            // Back past the newest message: the draft returns, the browse ends.
            key(KeyCode::Down, KeyModifiers::ALT),
            HeadlessStep::WaitMs(200),
            ctrl('c'),
            HeadlessStep::WaitMs(100),
        ],
    );
    let pauses: Vec<(&str, Value)> = commands
        .iter()
        .filter_map(|command| match command["type"].as_str() {
            Some("acquire_session_input_pause") => Some(("acquire", command["leaseKey"].clone())),
            Some("release_session_input_pause") => Some(("release", command["pauseId"].clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        pauses,
        vec![
            ("acquire", json!("tui-queue-edit")),
            ("release", json!("pause-1"))
        ]
    );
}

/// With a daemon that can move the running cell to the background, send-now
/// keeps the work running (Claude Code's send-now): the draft is steered and
/// the cell detached, and nothing is aborted.
#[test]
fn ctrl_enter_moves_the_running_cell_instead_of_interrupting() {
    let (_frames, commands, _copies) = run_with(
        true,
        vec!["parked"],
        1,
        true,
        vec![
            wait_for("parked"),
            HeadlessStep::Type("right now".to_string()),
            key(KeyCode::Enter, KeyModifiers::CONTROL),
            HeadlessStep::WaitMs(300),
        ],
    );
    let kinds = types(&commands);
    let steer = kinds
        .iter()
        .position(|kind| kind == "prompt")
        .expect("the draft was steered");
    let detach = kinds
        .iter()
        .position(|kind| kind == "detach_running_cell")
        .expect("the running cell was moved");
    assert!(steer < detach, "{kinds:?}");
    assert!(
        !kinds.iter().any(|kind| kind.starts_with("abort")),
        "nothing was interrupted: {kinds:?}"
    );
}

/// Ctrl+Enter while a cell that blocks the loop is running: the daemon reports
/// the cell could not be moved (`reason:"blocked"`). The send-now path must
/// leave the running work alone -- the message steers, the detach is attempted,
/// and nothing is aborted (the step keeps running; Ctrl+C is the escape).
#[test]
fn ctrl_enter_leaves_a_blocked_cell_running_instead_of_aborting() {
    let (_frames, commands, _copies) = run_detach(
        true,
        vec!["parked"],
        1,
        true,
        DetachAnswer::Blocked,
        vec![
            wait_for("parked"),
            HeadlessStep::Type("right now".to_string()),
            key(KeyCode::Enter, KeyModifiers::CONTROL),
            HeadlessStep::WaitMs(300),
        ],
    );
    let kinds = types(&commands);
    assert!(
        kinds.iter().any(|kind| kind == "detach_running_cell"),
        "a detach was attempted: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|kind| kind.starts_with("abort")),
        "a blocked cell is left running, not interrupted: {kinds:?}"
    );
}

/// Ctrl+Enter while the model is writing its reply (no cell to move): the
/// reply is cut so the queued message goes out (Claude Code's promote-to-now),
/// and the abort says it is send-now's so the cut row is labeled as such.
#[test]
fn ctrl_enter_cuts_a_reply_being_written_as_send_now() {
    let (_frames, commands, _copies) = run_detach(
        true,
        vec!["parked"],
        1,
        true,
        DetachAnswer::NoCell,
        vec![
            wait_for("parked"),
            HeadlessStep::Type("right now".to_string()),
            key(KeyCode::Enter, KeyModifiers::CONTROL),
            HeadlessStep::WaitMs(300),
        ],
    );
    let aborts: Vec<&Value> = commands
        .iter()
        .filter(|command| {
            command["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("abort"))
        })
        .collect();
    assert_eq!(
        aborts
            .iter()
            .map(|command| (command["type"].clone(), command["sendNow"].clone()))
            .collect::<Vec<_>>(),
        vec![(json!("abort_and_send_queued"), json!(true))]
    );
}

/// Ctrl+Enter whose detach request fails: the turn is left running with the
/// message queued (Claude Code: "Send now isn't available here"), never aborted.
#[test]
fn ctrl_enter_never_aborts_when_the_detach_request_fails() {
    let (_frames, commands, _copies) = run_detach(
        true,
        vec!["parked"],
        1,
        true,
        DetachAnswer::Fails,
        vec![
            wait_for("parked"),
            HeadlessStep::Type("right now".to_string()),
            key(KeyCode::Enter, KeyModifiers::CONTROL),
            HeadlessStep::WaitMs(300),
        ],
    );
    let kinds = types(&commands);
    assert!(
        kinds.iter().any(|kind| kind == "detach_running_cell")
            && !kinds.iter().any(|kind| kind.starts_with("abort")),
        "{kinds:?}"
    );
}

#[test]
fn ctrl_enter_sends_now_and_ctrl_alt_x_clears_the_queue() {
    let (_frames, commands, _copies) = run(
        true,
        vec!["parked"],
        1,
        vec![
            wait_for("parked"),
            HeadlessStep::Type("right now".to_string()),
            key(KeyCode::Enter, KeyModifiers::CONTROL),
            HeadlessStep::WaitMs(200),
            key(
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            HeadlessStep::WaitMs(200),
        ],
    );
    let kinds = types(&commands);
    let steer = kinds
        .iter()
        .position(|kind| kind == "prompt")
        .expect("the draft was steered");
    let abort = kinds
        .iter()
        .position(|kind| kind == "abort_and_send_queued")
        .expect("the turn was interrupted to send it now");
    assert!(steer < abort, "{kinds:?}");
    assert!(kinds.iter().any(|kind| kind == "clear_queue"), "{kinds:?}");
}

#[test]
fn ctrl_c_with_a_selection_copies_and_does_nothing_else() {
    let (frames, commands, copies) = run(
        true,
        Vec::new(),
        1,
        vec![
            HeadlessStep::Type("copy me".to_string()),
            wait_for("copy me"),
            key(
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            ctrl('c'),
            HeadlessStep::WaitMs(100),
            // Then a plain press interrupts the running turn (ending it here).
            key(KeyCode::Right, KeyModifiers::NONE),
            ctrl('c'),
            HeadlessStep::WaitMs(100),
        ],
    );
    assert_eq!(copies, vec!["copy me".to_string()]);
    assert_eq!(
        types(&commands),
        vec!["abort_and_send_queued"],
        "only the second press interrupted"
    );
    assert!(
        frames.iter().rev().any(|frame| frame.contains("copy me")),
        "the copy kept the editor text"
    );
}

#[test]
fn search_jumps_to_a_match_and_escape_clears_it() {
    let (frames, _commands, _copies) = run(
        false,
        Vec::new(),
        0,
        vec![
            wait_for("filler prompt number 39"),
            HeadlessStep::Submit("/search NEEDLE".to_string()),
            wait_for("Match 1 of 1"),
            key(KeyCode::Esc, KeyModifiers::NONE),
            HeadlessStep::WaitMs(100),
            HeadlessStep::Submit("/search nothing-like-this".to_string()),
            wait_for("No matches"),
        ],
    );
    let opened = frames
        .iter()
        .find(|frame| frame.contains("filler prompt number 39"))
        .expect("the session opened at its tail");
    assert!(
        !opened.contains("the needle is here"),
        "the match starts off screen"
    );
    let found = frames
        .iter()
        .find(|frame| frame.contains("Match 1 of 1"))
        .expect("the search reported its match");
    assert!(
        found.contains("the needle is here"),
        "the match scrolled into view:\n{found}"
    );
}
