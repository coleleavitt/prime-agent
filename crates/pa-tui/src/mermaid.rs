//! Mermaid diagrams as Unicode box-drawing art: a port of lovely-mermaid 0.3.3 (the
//! renderer the fork's TS product moved to in `a358fd19e`) plus the fork's diagram policy —
//! a too-wide flowchart redrawn on the other axis, a note giving the columns a diagram
//! needs, and art drawn with its warnings listed beside it.
//!
//! [`layout`] decides how one fence is shown; `markdown::mermaid_blocks` (assistant text,
//! custom messages, `/btw` answers) and [`crate::diagram::text_segments`] (agent messages)
//! paint it.

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

pub(crate) use policy::{Layout, Notice, NoticeLevel, layout};
#[cfg(test)]
pub(crate) use render::Art;
pub(crate) use render::{ArtSpan, Role};
