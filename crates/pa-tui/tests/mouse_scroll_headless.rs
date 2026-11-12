//! Headless e2e for the mouse-wheel scroll surface (TS
//! `handleFullscreenInput`): wheel up/down scroll the transcript one row
//! per wheel report (upstream #887), the wheel is consumed without scrolling while a
//! picker owns the frame, and tracking-off reports are consumed either way.
#![cfg(unix)]
#![warn(clippy::allow_attributes_without_reason)]

mod support;

use std::sync::{Mutex, MutexGuard};

/// Mouse tracking is process-global state, so the headless runs serialize (each asserts on the
/// tracking-active branch it drives).
static RUN_LOCK: Mutex<()> = Mutex::new(());

fn run_lock() -> MutexGuard<'static, ()> {
    match RUN_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

use pa_tui::interactive::{
    HeadlessPlan,
    HeadlessStep,
    InteractiveOptions,
    UiMode,
    run_interactive,
};
use serde_json::{Value, json};
use support::{
    MockSupervisor,
    Request,
    attach_response,
    create_response,
    detach_response,
    options,
    serve_connection,
    session_stats_response,
    write_json,
};

/// The SGR wheel-up / wheel-down press reports a real terminal sends.
const WHEEL_UP: &str = "\x1b[<64;10;10M";
const WHEEL_DOWN: &str = "\x1b[<65;10;10M";

impl MockSupervisor {
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
            match kind {
                "create" => {
                    write_json(writer, &create_response(id));
                }
                "attach" => {
                    write_json(writer, &attach_data(id));
                }
                "get_session_stats" => {
                    write_json(writer, &session_stats_response(id));
                }
                "prompt" => {
                    write_json(
                        writer,
                        &json!({
                            "type": "response",
                            "id": id,
                            "command": "prompt",
                            "success": true,
                        }),
                    );
                }
                "detach" => {
                    write_json(writer, &detach_response(id));
                }
                _ => return false,
            }
            true
        });
    }
}

/// The slim attach result: 40 alternating one-row messages — the transcript is far taller than the
/// 30-row frame.
fn attach_data(id: &str) -> Value {
    let messages: Vec<Value> = (0..40)
        .map(|index| {
            if index % 2 == 0 {
                json!({ "role": "user", "content": [{ "type": "text", "text": format!("row {index}") }] })
            } else {
                json!({
                    "role": "assistant",
                    "content": [{ "type": "text", "text": format!("answer {index}") }],
                })
            }
        })
        .collect();
    let mut response = attach_response(id, "mouse session");
    response["data"]["snapshot"]["messages"] = json!(messages);
    response
}

/// Run the headless plan and return the captured frames. Holds the run lock: mouse tracking is
/// process-global.
fn run_plan(steps: Vec<HeadlessStep>, fullscreen_mouse: bool) -> Vec<String> {
    let _guard = run_lock();
    // The ambient TMUX variable shifts the paused-frame geometry the assertions reason about; scrub
    // it so the run is the same inside tmux and out.
    pa_types::env::remove_var("TMUX");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let socket = dir.path().join("tui.sock");
    let supervisor = MockSupervisor::bind(&socket);
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
        .block_on(run_interactive(
            InteractiveOptions {
                fullscreen_mouse,
                ..options(socket)
            },
            UiMode::Headless(plan),
        ))
        .expect("interactive run");
    let _ = handle.join();
    outcome.frames
}

/// Wheel turns scroll the transcript: up pauses tail-following and drops the newest message
/// from the window; wheel-down scrolls back, and reaching the bottom resumes following.
#[test]
fn wheel_turns_scroll_the_transcript() {
    let steps = vec![
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
        // Eighteen lines up clears the bottom rows of the tail (each user/assistant pair renders
        // several rows).
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
        // Wheel-down turns scroll back: following resumes (extra turns clamp at the bottom).
        HeadlessStep::Mouse(WHEEL_DOWN.to_string()),
        HeadlessStep::Mouse(WHEEL_DOWN.to_string()),
        HeadlessStep::Mouse(WHEEL_DOWN.to_string()),
        HeadlessStep::Mouse(WHEEL_DOWN.to_string()),
        HeadlessStep::Mouse(WHEEL_DOWN.to_string()),
        HeadlessStep::Mouse(WHEEL_DOWN.to_string()),
        HeadlessStep::Mouse(WHEEL_DOWN.to_string()),
    ];
    let frames = run_plan(steps, true);
    assert!(!frames.is_empty(), "frames were captured");
    let all = frames.join("\n");
    assert!(
        all.contains("answer 39"),
        "the snapshot transcript's tail rendered:\n{all}"
    );
    assert!(
        all.contains("to follow"),
        "wheel-up paused tail-following (the follow hint rendered):\n{all}"
    );

    let paused = frames
        .iter()
        .rfind(|frame| frame.contains("to follow"))
        .expect("a paused frame");
    assert!(
        !paused.contains("answer 39"),
        "the scrolled-up window dropped the newest rows:\n{paused}"
    );
    let tail = frames
        .iter()
        .rfind(|frame| !frame.contains("to follow") && frame.contains("row 38"))
        .expect("a resumed tail frame");
    assert!(
        tail.contains("row 38"),
        "wheel-down returned the window to the tail:\n{tail}"
    );
}

/// One wheel report moves the transcript window by exactly one row (upstream #887; TS v0.9.8
/// moved three, which jumps on terminals that send several reports per notch): every transcript
/// line of the paused frame is the tail frame's line one row higher.
#[test]
fn one_wheel_report_scrolls_one_row() {
    let steps = vec![
        HeadlessStep::WaitRender {
            needle: "answer 39".to_string(),
            timeout_ms: 5_000,
        },
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
    ];
    let frames = run_plan(steps, true);
    let tail = frames
        .iter()
        .rfind(|frame| !frame.contains("to follow") && frame.contains("answer 39"))
        .expect("a tail frame");
    let paused = frames
        .iter()
        .find(|frame| frame.contains("to follow"))
        .expect("a paused frame");
    let row_of = |frame: &str, needle: &str| {
        frame
            .lines()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} is on screen:\n{frame}"))
    };
    for needle in ["row 34", "answer 35", "row 36"] {
        assert_eq!(
            row_of(paused, needle),
            row_of(tail, needle) + 1,
            "one wheel report moved {needle:?} by one row:\ntail:\n{tail}\npaused:\n{paused}"
        );
    }
}

/// The TS overlay-focus gate: the wheel is consumed while the picker owns
/// the frame.
#[test]
fn wheel_is_ignored_while_a_picker_owns_the_frame() {
    let steps = vec![
        HeadlessStep::Submit("/model".to_string()),
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
        HeadlessStep::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        )),
    ];
    let frames = run_plan(steps, true);
    let all = frames.join("\n");
    assert!(
        all.contains("row 38"),
        "the transcript tail stayed mounted through the picker cycle:\n{all}"
    );
    // The picker cycled without any paused frame: every frame that shows the transcript sits at the
    // tail.
    assert!(
        !all.contains("to follow"),
        "the wheel never scrolled behind the picker:\n{all}"
    );
    let tail = frames.last().expect("a frame after the picker closed");
    assert!(
        tail.contains("row 38"),
        "the post-picker frame is the unscrolled tail:\n{tail}"
    );
}

#[test]
fn wheel_reports_are_consumed_when_tracking_is_disabled() {
    let steps = vec![
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
        HeadlessStep::Mouse(WHEEL_UP.to_string()),
    ];
    let frames = run_plan(steps, false);
    let all = frames.join("\n");
    assert!(
        !all.contains("to follow"),
        "no scroll with tracking disabled:\n{all}"
    );
    let tail = frames.last().expect("a frame");
    assert!(
        tail.contains("row 38"),
        "the tail frame stayed at the bottom:\n{tail}"
    );
}
