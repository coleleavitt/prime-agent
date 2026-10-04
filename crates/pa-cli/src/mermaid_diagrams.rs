//! The fork's Mermaid diagrams (`pa-mermaid`) behind the TUI's diagram renderer seam
//! (`pa_tui::diagram`): lovely-mermaid 0.3.3 art, a too-wide flowchart redrawn on the
//! other axis, the columns a diagram needs when nothing fits, art drawn with its warnings
//! listed, and diagrams in agent messages, custom messages, and `/btw` answers (TS
//! `a358fd19e`).

use pa_tui::diagram::{
    DiagramLayout, DiagramNotice, DiagramRenderer, DiagramRole, DiagramSpan, DiagramSurface,
    NoticeLevel,
};

/// `pa_mermaid::layout` as the TUI's diagram renderer.
struct MermaidDiagrams;

fn role(role: pa_mermaid::Role) -> DiagramRole {
    match role {
        pa_mermaid::Role::Border => DiagramRole::Border,
        pa_mermaid::Role::Text => DiagramRole::Text,
        pa_mermaid::Role::Edge => DiagramRole::Edge,
        pa_mermaid::Role::EdgeLabel => DiagramRole::EdgeLabel,
        pa_mermaid::Role::Title => DiagramRole::Title,
        pa_mermaid::Role::None => DiagramRole::None,
    }
}

fn notices(notices: Vec<pa_mermaid::Notice>) -> Vec<DiagramNotice> {
    notices
        .into_iter()
        .map(|notice| DiagramNotice {
            level: match notice.level {
                pa_mermaid::NoticeLevel::Info => NoticeLevel::Info,
                pa_mermaid::NoticeLevel::Warning => NoticeLevel::Warning,
            },
            text: notice.text,
        })
        .collect()
}

impl DiagramRenderer for MermaidDiagrams {
    fn layout(&self, source: &str, available_width: usize, streaming: bool) -> DiagramLayout {
        match pa_mermaid::layout(source, available_width, streaming) {
            pa_mermaid::Layout::Art { art, notices: n } => DiagramLayout::Rows {
                rows: art
                    .rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|span| DiagramSpan {
                                text: span.text,
                                role: role(span.role),
                            })
                            .collect()
                    })
                    .collect(),
                notices: notices(n),
            },
            pa_mermaid::Layout::Source { notices: n } => DiagramLayout::Source {
                notices: notices(n),
            },
        }
    }

    /// Agent messages (received and sent), custom messages, and `/btw` answers draw
    /// diagrams too.
    fn draws_on(&self, surface: DiagramSurface) -> bool {
        matches!(
            surface,
            DiagramSurface::AgentMessage
                | DiagramSurface::CustomMessage
                | DiagramSurface::SideAnswer
        )
    }
}

/// Install the renderer, measuring art with the TUI's own grapheme widths so a diagram
/// judged to fit really fits the transcript row. Idempotent.
pub(crate) fn install() {
    pa_mermaid::set_width_measure(pa_tui::width::str_width);
    // A second install (tests, a repeated call) keeps the first.
    let _ = pa_tui::diagram::install_diagram_renderer(Box::new(MermaidDiagrams));
}
