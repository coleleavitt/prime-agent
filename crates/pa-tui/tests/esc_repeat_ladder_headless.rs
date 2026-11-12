//! Headless e2e for the double-Escape repeat ladder's termination
//! (operator report 2026-09-29): repeated Escape at the empty editor
//! converges to the inert empty state — the repeat's tree action is one
//! shot per input chain, and any non-Escape key re-arms the gesture. On a
//! daemon with the `rewind` capability the gesture opens `/rewind` (Claude
//! Code's Esc Esc) instead of the tree.
#![cfg(unix)]
// The mock supervisor's request loop is one arm per wire command; splitting adds indirection.
#![allow(clippy::too_many_lines)]

mod support;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pa_tui::interactive::{HeadlessPlan, HeadlessStep, InteractiveOptions};
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

impl MockSupervisor {
    /// Serve one connection: the tree answer carries two entries so the selector actually mounts
    /// (the empty-tree arm only notes "No entries"). Every command lands in `seen`.
    fn serve(self, capabilities: &[&str], seen: &Mutex<Vec<Value>>) {
        let (stream, _) = self.listener.accept().expect("accept");
        serve_connection(
            stream,
            capabilities,
            |writer, Request { id, kind, command }| {
                seen.lock().unwrap().push(command);
                match kind {
                    "create" => {
                        write_json(writer, &create_response(id));
                    }
                    "attach" => {
                        write_json(writer, &attach_response(id, "esc ladder"));
                    }
                    "get_commands" => {
                        write_json(
                            writer,
                            &json!({
                                "type": "response",
                                "id": id,
                                "command": "get_commands",
                                "success": true,
                                "data": { "commands": [] },
                            }),
                        );
                    }
                    "get_session_stats" => {
                        write_json(writer, &session_stats_response(id));
                    }
                    "get_session_tree" => {
                        write_json(
                            writer,
                            &json!({
                                "type": "response",
                                "id": id,
                                "command": "get_session_tree",
                                "success": true,
                                "data": {
                                    "flatNodes": [
                                        {
                                            "entry": {
                                                "type": "message",
                                                "id": "n0",
                                                "parentId": null,
                                                "timestamp": "2024-01-01T00:00:00.000Z",
                                                "message": {
                                                    "role": "user",
                                                    "content": "first",
                                                    "timestamp": 0,
                                                },
                                            },
                                        },
                                        {
                                            "entry": {
                                                "type": "message",
                                                "id": "n1",
                                                "parentId": "n0",
                                                "timestamp": "2024-01-01T00:00:01.000Z",
                                                "message": {
                                                    "role": "assistant",
                                                    "content": "second",
                                                    "timestamp": 1,
                                                },
                                            },
                                        },
                                    ],
                                    "leafId": "n1",
                                },
                            }),
                        );
                    }
                    "get_rewind_checkpoints" => {
                        write_json(
                            writer,
                            &json!({
                                "type": "response",
                                "id": id,
                                "command": "get_rewind_checkpoints",
                                "success": true,
                                "data": {
                                    "codeRestore": true,
                                    "checkpoints": [{
                                        "entryId": "n0",
                                        "text": "first",
                                        "timestamp": 1,
                                        "code": {
                                            "filesChanged": ["/tmp/work/main.rs"],
                                            "insertions": 2,
                                            "deletions": 5,
                                            "externallyModified": [],
                                            "unrestorable": [],
                                        },
                                    }],
                                },
                            }),
                        );
                    }
                    "rewind" => {
                        write_json(
                            writer,
                            &json!({
                                "type": "response",
                                "id": id,
                                "command": "rewind",
                                "success": true,
                                "data": {
                                    "code": {
                                        "restored": ["/tmp/work/main.rs"],
                                        "deleted": [],
                                        "skipped": [],
                                    },
                                    "conversation": { "cancelled": false, "editorText": "first" },
                                },
                            }),
                        );
                    }
                    "detach" => {
                        write_json(writer, &detach_response(id));
                    }
                    _ => return false,
                }
                true
            },
        );
    }
}

/// A minimal settings seam for the harness: every getter returns its TS default, writes succeed
/// without persistence.
struct StubSettings;

impl pa_tui::client_settings::ClientSettings for StubSettings {
    fn compaction_threshold_percent(&self) -> f64 {
        95.0
    }
    fn set_compaction_threshold_percent(&self, _percent: f64) -> Result<()> {
        Ok(())
    }

    fn theme(&self) -> Option<String> {
        None
    }
    fn set_theme(&self, _theme: &str) -> Result<()> {
        Ok(())
    }
    fn show_images(&self) -> bool {
        true
    }
    fn set_show_images(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn clear_on_shrink(&self) -> bool {
        false
    }
    fn set_clear_on_shrink(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn show_terminal_progress(&self) -> bool {
        false
    }
    fn set_show_terminal_progress(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn image_auto_resize(&self) -> bool {
        true
    }
    fn set_image_auto_resize(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn block_images(&self) -> bool {
        false
    }
    fn set_block_images(&self, _blocked: bool) -> Result<()> {
        Ok(())
    }
    fn image_model(&self) -> Option<String> {
        None
    }
    fn enable_skill_commands(&self) -> bool {
        true
    }
    fn set_enable_skill_commands(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn enable_builtin_skills(&self) -> bool {
        true
    }
    fn set_enable_builtin_skills(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn show_hardware_cursor(&self) -> bool {
        false
    }
    fn set_show_hardware_cursor(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn editor_padding_x(&self) -> u64 {
        0
    }
    fn set_editor_padding_x(&self, _padding: u64) -> Result<()> {
        Ok(())
    }
    fn autocomplete_max_visible(&self) -> u64 {
        5
    }
    fn set_autocomplete_max_visible(&self, _max: u64) -> Result<()> {
        Ok(())
    }
    fn quiet_startup(&self) -> bool {
        false
    }
    fn set_quiet_startup(&self, _quiet: bool) -> Result<()> {
        Ok(())
    }
    fn idle_eviction_minutes(&self) -> String {
        "90".to_string()
    }
    fn set_idle_eviction_minutes(&self, _value: &str) -> Result<()> {
        Ok(())
    }
    fn mermaid_rendering_mode(&self) -> String {
        "streaming".to_string()
    }
    fn set_mermaid_rendering_mode(&self, _mode: &str) -> Result<()> {
        Ok(())
    }
    fn tree_filter_mode(&self) -> String {
        "user-only".to_string()
    }
    fn set_tree_filter_mode(&self, _mode: &str) -> Result<()> {
        Ok(())
    }
    fn default_service_tier(&self) -> String {
        "auto".to_string()
    }
    fn set_default_service_tier(&self, _tier: &str) -> Result<()> {
        Ok(())
    }
    fn chat_detail(&self) -> String {
        "collapsed".to_string()
    }
    fn set_chat_detail(&self, _detail: &str) -> Result<()> {
        Ok(())
    }
    fn factory_enabled(&self) -> bool {
        false
    }
    fn set_factory_enabled(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn warnings_anthropic_extra_usage(&self) -> bool {
        true
    }
    fn set_warnings_anthropic_extra_usage(&self, _enabled: bool) -> Result<()> {
        Ok(())
    }
    fn update_channel(&self) -> Option<String> {
        None
    }
    fn set_update_channel(&self, _channel: &str) -> Result<()> {
        Ok(())
    }
    fn telemetry_status(&self) -> String {
        String::new()
    }
    fn set_telemetry_enabled(&self, _enabled: bool) -> Result<String> {
        Ok(String::new())
    }
    fn effective_update_channel(&self, _version: &str) -> String {
        "stable".to_string()
    }
}

fn esc() -> HeadlessStep {
    HeadlessStep::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
}

/// Run one plan against a fresh mock supervisor and return the captured frames. The headless
/// capture dedupes consecutive identical frames, so each tree-selector mount lands as its own
/// frame.
fn run_plan(steps: Vec<HeadlessStep>) -> Vec<String> {
    run_plan_with(Vec::new(), steps).0
}

/// [`run_plan`] against a daemon advertising `capabilities`; also returns the
/// commands the client sent.
fn run_plan_with(
    capabilities: Vec<&'static str>,
    steps: Vec<HeadlessStep>,
) -> (Vec<String>, Vec<Value>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let served = Arc::clone(&seen);
    let outcome = run_headless(
        HeadlessPlan {
            steps,
            width: 100,
            height: 30,
        },
        |socket| InteractiveOptions {
            client_settings: Some(std::sync::Arc::new(StubSettings)),
            ..options(socket)
        },
        move |socket| {
            let supervisor = MockSupervisor::bind(socket);
            move || supervisor.serve(&capabilities, &served)
        },
    );
    let seen = seen.lock().unwrap().clone();
    (outcome.frames, seen)
}

/// The tree-selector pane's title row (the mount's needle).
const TREE_PANE: &str = "Session Tree";

/// Sixty Escape presses at the empty editor: the repeat's tree action fires exactly once and the
/// ladder terminates instead of cycling the selector every second press.
#[test]
fn repeated_escape_converges_to_the_inert_empty_state() {
    let mut steps = vec![HeadlessStep::WaitMs(300)];
    for _ in 0..60 {
        steps.push(esc());
    }
    steps.push(HeadlessStep::WaitMs(200));
    let frames = run_plan(steps);

    let tree_frames = frames
        .iter()
        .filter(|frame| frame.contains(TREE_PANE))
        .count();
    assert_eq!(
        tree_frames, 1,
        "the double-Esc gesture opens the tree selector exactly once; a cycling ladder reopens it every second press"
    );

    let last = frames.last().expect("the run captured frames");
    assert!(
        !last.contains(TREE_PANE),
        "the final frame is the inert empty editor, not the reopened selector: {last}"
    );
    assert!(
        last.contains('>') && !last.contains("No entries in session"),
        "the final frame shows the empty prompt, not a tree or note row: {last}"
    );
}

/// A non-Escape key re-arms the gesture: after the converged inert tail, one arrow press starts a
/// fresh input chain and the next double-Esc opens the selector again.
#[test]
fn a_non_escape_key_re_arms_the_double_escape_gesture() {
    let mut steps = vec![HeadlessStep::WaitMs(300)];
    for _ in 0..60 {
        steps.push(esc());
    }
    // A real interaction with no side effect: the editor's right arrow
    // (a cursor motion that stays put at the empty editor). Down would
    // move the focus into the activity dock below the prompt.
    steps.push(HeadlessStep::Key(KeyEvent::new(
        KeyCode::Right,
        KeyModifiers::NONE,
    )));
    steps.push(esc());
    steps.push(esc());
    steps.push(esc());
    steps.push(HeadlessStep::WaitMs(200));
    let frames = run_plan(steps);

    let tree_frames = frames
        .iter()
        .filter(|frame| frame.contains(TREE_PANE))
        .count();
    assert_eq!(
        tree_frames, 2,
        "the gesture fires once per input chain: the first chain's open plus the re-armed second"
    );

    let last = frames.last().expect("the run captured frames");
    assert!(
        !last.contains(TREE_PANE),
        "the second chain's selector also dismisses back to the empty editor: {last}"
    );
}

fn enter() -> HeadlessStep {
    HeadlessStep::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
}

/// The `/rewind` pane's description row (the mount's needle).
const REWIND_PANE: &str = "Restore the code and/or conversation to the point before";

/// With the `rewind` capability, Esc Esc on the empty editor opens Claude Code's rewind picker:
/// Enter picks the prompt, Enter confirms the focused "Restore code and conversation", the daemon
/// gets the restore, and the prompt's text comes back to the editor.
#[test]
fn double_escape_opens_rewind_and_restores_code_and_conversation() {
    let steps = vec![
        HeadlessStep::WaitMs(300),
        esc(),
        esc(),
        HeadlessStep::WaitMs(200),
        enter(),
        HeadlessStep::WaitMs(100),
        enter(),
        HeadlessStep::WaitMs(300),
    ];
    let (frames, seen) = run_plan_with(vec!["rewind"], steps);
    assert!(
        frames.iter().any(|frame| frame.contains(REWIND_PANE)),
        "the gesture mounts the rewind picker"
    );
    assert!(
        !frames.iter().any(|frame| frame.contains(TREE_PANE)),
        "not the tree selector"
    );
    assert!(
        frames.iter().any(|frame| frame.contains("main.rs +2 -5")),
        "the row carries its code summary"
    );
    assert!(
        frames
            .iter()
            .any(|frame| frame.contains("The code will be restored +2 -5 in main.rs.")),
        "the confirmation states the code restore"
    );
    let rewind = seen
        .iter()
        .find(|command| command["type"] == "rewind")
        .expect("the confirmed choice reached the daemon");
    assert_eq!(rewind["entryId"], "n0");
    assert_eq!(rewind["restoreCode"], true);
    assert_eq!(rewind["restoreConversation"], true);
    assert_eq!(rewind["summarize"], false);
    let last = frames.last().expect("frames");
    assert!(
        last.contains("Restored the code: 1 file"),
        "the outcome is noted: {last}"
    );
    assert!(!last.contains(REWIND_PANE), "the picker closed: {last}");
}
