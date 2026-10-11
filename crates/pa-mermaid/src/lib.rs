//! Mermaid diagrams for the fork's TUI: a port of lovely-mermaid 0.3.3 (the renderer the
//! fork's TS product moved to in `a358fd19e`) plus the fork's diagram policy — a
//! too-wide flowchart redrawn on the other axis, a note giving the columns a diagram
//! needs, and art drawn with its warnings listed beside it.
//!
//! The native TUI keeps its own grok-mermaid 0.2.3 renderer; `pa-cli` installs this
//! crate's [`layout`] into `pa_tui::diagram`'s renderer seam when the `mermaid` feature is
//! on. See the crate README for the contract.

// Pedantic-gate exceptions: the port keeps the package's JS-number arithmetic in `i64` and
// narrows structurally bounded values (canvas coordinates under `MAX_CANVAS_CELLS`, label
// widths, row counts); guarded conversions would add panic paths the bounds guarantee away.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// The layout and parse routines follow the package's functions one for one; splitting them
// would break the line-by-line correspondence the parity review reads against.
#![allow(clippy::too_many_lines)]

mod policy;
mod render;

pub use policy::{Axis, Layout, Notice, NoticeLevel, Rotated, layout, rotate_flowchart};
pub use render::{Art, ArtSpan, DiagramKind, Role, diagram_kind, render, render_cached};

/// Install the display width of one grapheme cluster the art is measured with, so a
/// diagram judged to fit really fits the host's rows. The first install wins (`false`
/// after); without one the package's own `unicode-width` rule applies. Install before the
/// first render: renders are cached by source.
pub fn set_width_measure(measure: fn(&str) -> usize) -> bool {
    render::width::set_measure(measure)
}
