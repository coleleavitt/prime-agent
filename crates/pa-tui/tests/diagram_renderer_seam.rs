//! The diagram renderer seam (`pa_tui::diagram`) with a stub renderer installed: whole
//! frames replayed from a session draw the stub's rows and notices in assistant text, and —
//! because the stub opts in — in custom messages, agent messages, and `/btw` answers,
//! all under the `markdown.mermaid` mode. (This binary installs the process-wide renderer;
//! the crate's own tests and every other binary run without one.)

use std::sync::Once;

use pa_tui::chat::Detail;
use pa_tui::diagram::{
    AlreadyInstalled,
    DiagramLayout,
    DiagramNotice,
    DiagramRenderer,
    DiagramRole,
    DiagramSpan,
    DiagramSurface,
    NoticeLevel,
    install_diagram_renderer,
};
use pa_tui::markdown::MermaidMode;
use pa_tui::session::{JsonlSessionStream, SessionStream, parse_jsonl};
use pa_tui::side_question::{SideQuestionPane, SideQuestionTurn};
use pa_tui::theme::{ColorMode, Theme};
use pa_tui::view::AgentView;

/// Draws one row naming the fence's first word and the width, then one notice.
struct Stub;

impl DiagramRenderer for Stub {
    fn layout(&self, source: &str, available_width: usize, streaming: bool) -> DiagramLayout {
        let kind = source.split_whitespace().next().unwrap_or("");
        DiagramLayout::Rows {
            rows: vec![vec![DiagramSpan {
                text: format!("<{kind} {available_width} {streaming}>"),
                role: DiagramRole::Edge,
            }]],
            notices: vec![DiagramNotice {
                level: NoticeLevel::Info,
                text: "stub note".to_owned(),
            }],
            adapted: false,
        }
    }

    fn draws_on(&self, _: DiagramSurface) -> bool {
        true
    }
}

fn install_stub() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| install_diagram_renderer(Box::new(Stub)).expect("first install"));
}

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
    install_stub();
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

/// The frame rows that carry `needle`, trimmed.
fn rows_with(frame: &[String], needle: &str) -> Vec<String> {
    frame
        .iter()
        .filter(|row| row.contains(needle))
        .map(|row| row.trim().to_string())
        .collect()
}

#[test]
fn the_first_install_wins() {
    install_stub();
    assert_eq!(
        install_diagram_renderer(Box::new(Stub)),
        Err(AlreadyInstalled)
    );
}

#[test]
fn an_installed_renderer_draws_on_every_surface_it_opts_into() {
    let frame = frame_text(MermaidMode::Final, Detail::All);
    // Assistant text (content width 58), the custom panel's branch body, and the agent
    // message body (60 - 4) each draw the stub row; every surface is settled.
    assert_eq!(
        rows_with(&frame, " false>"),
        vec![
            "<flowchart 58 false>",
            "<sequenceDiagram 56 false>",
            "<classDiagram 56 false>",
        ],
        "{frame:#?}"
    );
    assert_eq!(rows_with(&frame, "stub note").len(), 3, "{frame:#?}");
    // The paragraph closes after the notice: the text after the fence stands apart.
    let notice = frame
        .iter()
        .position(|row| row.trim() == "stub note")
        .expect("assistant notice");
    assert_eq!(frame[notice + 1], "");
    assert_eq!(frame[notice + 2].trim(), "Done.");
}

#[test]
fn the_off_mode_keeps_every_fence() {
    let frame = frame_text(MermaidMode::Off, Detail::All);
    assert_eq!(
        rows_with(&frame, " false>"),
        Vec::<String>::new(),
        "{frame:#?}"
    );
    assert_eq!(rows_with(&frame, "stub note"), Vec::<String>::new());
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

fn pane_rows(status: &str, mode: MermaidMode) -> Vec<String> {
    install_stub();
    let theme = Theme::builtin("prime", ColorMode::TrueColor);
    let rows = pane(status).render(&theme, 0, false, "Esc", 40, mode);
    rows.iter()
        .map(|row| {
            row.iter()
                .map(|span| span.content.as_str())
                .collect::<String>()
        })
        .filter(|row| row.contains("e>"))
        .map(|row| row.trim().to_owned())
        .collect()
}

#[test]
fn side_answers_draw_once_settled_in_final_mode_and_while_running_in_streaming_mode() {
    assert_eq!(
        pane_rows("running", MermaidMode::Final),
        Vec::<String>::new()
    );
    assert_eq!(
        pane_rows("complete", MermaidMode::Final),
        ["<flowchart 38 false>"]
    );
    assert_eq!(
        pane_rows("running", MermaidMode::Streaming),
        ["<flowchart 38 true>"]
    );
    assert_eq!(
        pane_rows("complete", MermaidMode::Off),
        Vec::<String>::new()
    );
}
