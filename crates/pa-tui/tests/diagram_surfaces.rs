//! Mermaid diagrams on every surface, whole frames replayed from a session: assistant
//! text, custom messages, agent messages, and `/btw` answers draw their `mermaid` fences,
//! all under the `markdown.mermaid` mode.

use pa_tui::chat::Detail;
use pa_tui::markdown::MermaidMode;
use pa_tui::session::{JsonlSessionStream, SessionStream, parse_jsonl};
use pa_tui::side_question::{SideQuestionPane, SideQuestionTurn};
use pa_tui::theme::{ColorMode, Theme};
use pa_tui::view::AgentView;

const SESSION: &str = concat!(
    r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"draw it"}],"timestamp":1}}"#,
    "\n",
    r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"The flow:\n```mermaid\nflowchart TD\n  A --> B\n```\nDone."}],"api":"faux:1","provider":"faux","model":"faux-1","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"stop","timestamp":2}}"#,
    "\n",
    r#"{"type":"custom_message","customType":"note","content":"Panel:\n\n```mermaid\nsequenceDiagram\n  A->>B: hi\n```","display":true}"#,
    "\n",
    r#"{"type":"custom_message","customType":"agent_message","content":"[agent-message from child:lane]","display":true,"details":{"id":"agentmsg_1","message":"Plan:\n```mermaid\nclassDiagram\n  A <|-- B\n```\nok","from":{"sessionName":"lane"},"fromRelationship":"child"}}"#,
    "\n",
);

fn frame_text(mode: MermaidMode, detail: Detail) -> Vec<String> {
    let entries = parse_jsonl(SESSION).expect("session parses");
    let mut stream = JsonlSessionStream::from_entries(entries);
    let mut view = AgentView::new(Theme::builtin("prime", ColorMode::TrueColor));
    view.mermaid_mode = mode;
    view.detail = detail;
    while let pa_tui::session::SessionEvent::Item(item) = stream.poll().expect("poll") {
        view.push(item);
    }
    pa_tui::app::render_frame_text(&mut view, 60, 60)
        .into_iter()
        .map(|row| row.trim_end().to_string())
        .collect()
}

/// The transcript from the assistant text's first row through the agent message's last.
fn transcript(frame: &[String]) -> &[String] {
    let start = frame
        .iter()
        .position(|row| row == " The flow:")
        .expect("assistant text row");
    let end = frame
        .iter()
        .position(|row| row == "    ok")
        .expect("agent message's last row");
    &frame[start..=end]
}

#[test]
fn every_surface_draws_its_diagrams() {
    let frame = frame_text(MermaidMode::Final, Detail::All);
    assert_eq!(
        transcript(&frame),
        [
            " The flow:",
            "  ┌───┐",
            "  │ A │",
            "  └─┬─┘",
            "    │",
            "    ▼",
            "  ┌───┐",
            "  │ B │",
            "  └───┘",
            "",
            " Done.",
            "",
            " [note]",
            " ╰─ Panel:",
            "",
            "    ┌───┐  ┌───┐",
            "    │ A │  │ B │",
            "    └─┬─┘  └─┬─┘",
            "      │      │",
            "      │  hi  │",
            "      ├─────▶│",
            "      │      │",
            "    ┌─┴─┐  ┌─┴─┐",
            "    │ A │  │ B │",
            "    └───┘  └───┘",
            "",
            " ✉ Agent message · ↓ lane",
            " ╰─ Plan:",
            "     ┌───┐",
            "     │ A │",
            "     └─△─┘",
            "       │",
            "       │",
            "     ┌───┐",
            "     │ B │",
            "     └───┘",
            "    ok",
        ],
        "{frame:#?}"
    );
}

#[test]
fn the_off_mode_keeps_every_fence() {
    let frame = frame_text(MermaidMode::Off, Detail::All);
    let drawn: Vec<&String> = frame.iter().filter(|row| row.contains("┌───┐")).collect();
    assert_eq!(drawn, Vec::<&String>::new(), "{frame:#?}");
    for header in ["flowchart TD", "sequenceDiagram", "classDiagram"] {
        assert!(
            frame.iter().any(|row| row.trim() == header),
            "{header}: {frame:#?}"
        );
    }
}

fn pane(status: &str) -> SideQuestionPane {
    SideQuestionPane {
        turns: vec![SideQuestionTurn {
            id: "q1".to_owned(),
            question: "draw it".to_owned(),
            answer: "```mermaid\nflowchart LR\n  A --> B\n```".to_owned(),
            status: status.to_owned(),
            error_message: None,
            local: false,
        }],
        ..SideQuestionPane::default()
    }
}

/// The answer's drawn edge rows.
fn pane_rows(status: &str, mode: MermaidMode) -> Vec<String> {
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let rows = pane(status).render(&theme, 0, false, "Esc", 40, mode);
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .filter(|row| row.contains("───▶"))
        .map(|row| row.trim().to_owned())
        .collect()
}

#[test]
fn side_answers_draw_once_settled_in_final_mode_and_while_running_in_streaming_mode() {
    let drawn = ["│ A ├───▶│ B │"];
    assert_eq!(
        pane_rows("running", MermaidMode::Final),
        Vec::<String>::new()
    );
    assert_eq!(pane_rows("complete", MermaidMode::Final), drawn);
    assert_eq!(pane_rows("running", MermaidMode::Streaming), drawn);
    assert_eq!(
        pane_rows("complete", MermaidMode::Off),
        Vec::<String>::new()
    );
}
