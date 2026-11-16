//! Headless e2e for the tray's context usage during a running turn: a model call that ends
//! while the run goes on refreshes `N (P%)` from `get_session_stats`, instead of the tray
//! holding the attach-time value until `agent_end`.
#![cfg(unix)]
#![warn(clippy::allow_attributes_without_reason)]

mod support;

use std::os::unix::net::UnixStream;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{HeadlessPlan, HeadlessStep};
use serde_json::{Value, json};
use support::{
    MockSupervisor,
    Request,
    attach_response,
    create_response,
    detach_response,
    options,
    run_headless,
    serve_connection,
    session_stats_response,
    write_json,
};

/// The context the first mid-run stats read reports: 54,321 of 200,000 tokens.
const MID_RUN_TRAY: &str = "54k (27%)";

fn mid_run_stats_response(id: &str) -> Value {
    json!({
        "type": "response",
        "id": id,
        "command": "get_session_stats",
        "success": true,
        "data": { "contextUsage": { "tokens": 54_321, "contextWindow": 200_000 } },
    })
}

fn event(writer: &mut UnixStream, payload: &Value) {
    write_json(
        writer,
        &json!({ "type": "session_event", "activeSessionId": "s1", "event": payload }),
    );
}

impl MockSupervisor {
    /// Serve one connection: the prompt starts a run whose first model call ends without ending
    /// the run; Esc's `abort` ends it. The first stats read answers the mid-run context, every
    /// later one the shared 1,200-token reply.
    fn serve(self) {
        let (stream, _) = self.listener.accept().expect("accept");
        let mut stats_reads = 0;
        serve_connection(stream, &[], |writer, Request { id, kind, .. }| {
            let reply = match kind {
                "create" => create_response(id),
                "attach" => attach_response(id, "tray session"),
                "get_session_stats" => {
                    stats_reads += 1;
                    if stats_reads == 1 {
                        mid_run_stats_response(id)
                    } else {
                        session_stats_response(id)
                    }
                }
                "prompt" | "abort" => {
                    json!({ "type": "response", "id": id, "command": kind, "success": true })
                }
                "detach" => detach_response(id),
                _ => return false,
            };
            write_json(writer, &reply);
            match kind {
                "prompt" => {
                    event(writer, &json!({ "type": "agent_start" }));
                    event(writer, &json!({ "type": "turn_start" }));
                    event(writer, &json!({ "type": "turn_end" }));
                }
                "abort" => event(writer, &json!({ "type": "agent_end" })),
                _ => {}
            }
            true
        });
    }
}

#[test]
fn a_model_call_ending_mid_run_refreshes_the_tray_context() {
    let outcome = run_headless(
        HeadlessPlan {
            steps: vec![
                HeadlessStep::Submit("start the run".to_string()),
                HeadlessStep::WaitRender {
                    needle: MID_RUN_TRAY.to_string(),
                    timeout_ms: 5000,
                },
                HeadlessStep::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                HeadlessStep::WaitIdle { timeout_ms: 5000 },
            ],
            width: 100,
            height: 30,
        },
        options,
        |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || supervisor.serve()
        },
    );
    assert!(
        outcome
            .frames
            .iter()
            .any(|frame| frame.contains(MID_RUN_TRAY)),
        "the tray showed the mid-run context:\n{}",
        outcome.frames.join("\n")
    );
}
