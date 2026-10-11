//! Replay-path Mermaid rendering, whole frame: a settled assistant message's `mermaid`
//! fence renders as the diagram (TS `components/mermaid.ts`, default `streaming` mode),
//! falls back to the fence when the terminal is too narrow for it, and stays a fence with
//! the setting off.
// Casts: structurally bounded terminal-layout arithmetic; guarded conversions add panic paths.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use pa_tui::markdown::MermaidMode;
use pa_tui::session::{JsonlSessionStream, SessionStream, parse_jsonl};
use pa_tui::theme::{ColorMode, Theme};
use pa_tui::view::AgentView;

const MERMAID_SESSION: &str = concat!(
    r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"draw it"}],"timestamp":1}}"#,
    "\n",
    r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"The flow:\n```mermaid\nflowchart TD\n  A[Start] --> B{Ready?}\n  B -->|yes| C[开始]\n```\nDone."}],"api":"faux:1","provider":"faux","model":"faux-1","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}}"#,
    "\n",
);

fn frame_text(mode: MermaidMode, width: u16, height: u16) -> Vec<String> {
    let entries = parse_jsonl(MERMAID_SESSION).expect("session parses");
    let mut stream = JsonlSessionStream::from_entries(entries);
    let mut view = AgentView::new(Theme::builtin("prime", ColorMode::TrueColor));
    view.mermaid_mode = mode;
    while let pa_tui::session::SessionEvent::Item(item) = stream.poll().expect("poll") {
        view.push(item);
    }
    pa_tui::app::render_frame_text(&mut view, width, height)
        .into_iter()
        .map(|row| row.trim_end().to_string())
        .collect()
}

/// The transcript rows from the assistant message's first row through its last.
fn assistant_rows(frame: &[String]) -> Vec<String> {
    let start = frame
        .iter()
        .position(|row| row == " The flow:")
        .expect("assistant text row");
    let end = frame
        .iter()
        .position(|row| row == " Done.")
        .expect("closing text row");
    frame[start..=end].to_vec()
}

/// The diagram the fence draws (10 columns wide), joined to the paragraph above and below
/// exactly as TS's rewritten markdown joins it (no blank rows: the fence had none).
const DIAGRAM_ROWS: [&str; 15] = [
    " The flow:",
    "  ┌───────┐",
    "  │ Start │",
    "  └───┬───┘",
    "      │",
    "      ▼",
    " ╭────────╮",
    " │ Ready? │",
    " ╰────┬───╯",
    "      │",
    "      ▼yes",
    "  ┌──────┐",
    "  │ 开始 │",
    "  └──────┘",
    " Done.",
];

#[test]
fn a_settled_mermaid_fence_renders_as_its_diagram() {
    let frame = frame_text(MermaidMode::Streaming, 60, 30);
    assert_eq!(
        assistant_rows(&frame),
        DIAGRAM_ROWS.map(String::from).to_vec()
    );
    // A 12-column terminal leaves exactly the diagram's 10 columns inside the margins.
    let snug = frame_text(MermaidMode::Streaming, 12, 30);
    assert_eq!(
        assistant_rows(&snug),
        DIAGRAM_ROWS.map(String::from).to_vec()
    );
}

#[test]
fn a_terminal_too_narrow_for_the_diagram_shows_the_fence() {
    // One column short of the diagram.
    let frame = frame_text(MermaidMode::Streaming, 11, 30);
    let rows = assistant_rows(&frame);
    // The fence renders as code, wrapped like any code block at this width.
    assert_eq!(
        rows,
        vec![
            " The flow:",
            "",
            "",
            " flowchart",
            " TD",
            "",
            " A[Start]",
            " -->",
            " B{Ready?}",
            "     B",
            " -->|yes|",
            " C[开始]",
            "",
            " Done.",
        ]
    );
}

#[test]
fn the_off_setting_keeps_the_fence() {
    let frame = frame_text(MermaidMode::Off, 60, 30);
    assert_eq!(
        assistant_rows(&frame),
        vec![
            " The flow:",
            "",
            "   flowchart TD",
            "     A[Start] --> B{Ready?}",
            "     B -->|yes| C[开始]",
            "",
            " Done.",
        ]
    );
}
