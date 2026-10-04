//! The diagram registry: one module per supported diagram type. Ported from
//! lovely-mermaid 0.3.3 `registry.ts` (Apache-2.0; see `LICENSE-lovely-mermaid`).
//!
//! `diagram_kind` and `render` both resolve through [`diagram_for`], so the header test
//! each parser gates on and the one `diagram_kind` reports are the same by construction.

mod class;
mod er;
mod flowchart;
mod gitgraph;
mod mindmap;
mod pie;
pub(super) mod sequence;
mod state;
mod timeline;

use super::canvas::Canvas;
use super::statements::{header_kind, statements_of};
use crate::DiagramKind;

/// A drawn diagram before row extraction.
pub(super) struct Drawn {
    pub(super) canvas: Canvas,
    /// Source the grammar could not read and dropped, and any size-cap truncation.
    pub(super) warnings: Vec<String>,
}

/// One registry entry: the kind, its header keywords (lowercased, matched exactly), and
/// its parse-and-lay-out entry point.
struct Diagram {
    kind: DiagramKind,
    headers: &'static [&'static str],
    render: fn(&str) -> Option<Drawn>,
}

const DIAGRAMS: [Diagram; 9] = [
    Diagram {
        kind: DiagramKind::Flowchart,
        headers: flowchart::HEADERS,
        render: flowchart::render,
    },
    Diagram {
        kind: DiagramKind::State,
        headers: state::HEADERS,
        render: state::render,
    },
    Diagram {
        kind: DiagramKind::Class,
        headers: class::HEADERS,
        render: class::render,
    },
    Diagram {
        kind: DiagramKind::Er,
        headers: er::HEADERS,
        render: er::render,
    },
    Diagram {
        kind: DiagramKind::Sequence,
        headers: sequence::HEADERS,
        render: sequence::render,
    },
    Diagram {
        kind: DiagramKind::Pie,
        headers: pie::HEADERS,
        render: pie::render,
    },
    Diagram {
        kind: DiagramKind::Mindmap,
        headers: mindmap::HEADERS,
        render: mindmap::render,
    },
    Diagram {
        kind: DiagramKind::Timeline,
        headers: timeline::HEADERS,
        render: timeline::render,
    },
    Diagram {
        kind: DiagramKind::GitGraph,
        headers: gitgraph::HEADERS,
        render: gitgraph::render,
    },
];

fn diagram_for(src: &str) -> Option<&'static Diagram> {
    let header = header_kind(&statements_of(src))?;
    DIAGRAMS
        .iter()
        .find(|d| d.headers.contains(&header.as_str()))
}

/// The kind `src`'s header declares (controls already stripped).
pub(super) fn kind_of(src: &str) -> Option<DiagramKind> {
    diagram_for(src).map(|d| d.kind)
}

/// Parse and lay out `src` (controls already stripped); `None` means nothing was drawn.
pub(super) fn draw(src: &str) -> Option<Drawn> {
    (diagram_for(src)?.render)(src)
}
