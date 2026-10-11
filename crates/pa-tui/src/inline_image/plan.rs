//! The placement plan: which previews a composed frame shows, where, and
//! how much of each.

use super::{Marker, parse_marker};
use crate::Line;

/// One preview's visible band in a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Visible {
    pub(crate) key: u64,
    /// The frame row of the band's first row.
    pub(crate) row: u16,
    pub(crate) column: u16,
    pub(crate) columns: u32,
    /// The block row the band starts at (non-zero: scrolled out at the top).
    pub(crate) first: u32,
    /// The band's height in rows.
    pub(crate) rows: u32,
    /// The whole block's height.
    pub(crate) total: u32,
}

impl Visible {
    pub(crate) fn whole(&self) -> bool {
        self.first == 0 && self.rows == self.total
    }
}

/// Whether a reserved row's image cells still hold nothing: an overlay,
/// toast, or follow hint painted over them hides that row's band.
fn cells_blank(line: &Line, tag: &Marker) -> bool {
    let cells = crate::width::slice_line_by_column(line, tag.column as usize, tag.columns as usize);
    cells
        .iter()
        .all(|span| crate::ansi::strip_ansi(&span.content).trim().is_empty())
}

/// Every preview `frame` names, once each, top to bottom (the placeholder
/// path: the cells draw the image, so no band needs planning).
pub(crate) fn markers(frame: &[Line]) -> Vec<Marker> {
    let mut seen: Vec<Marker> = Vec::new();
    for line in frame {
        if let Some(tag) = line.iter().find_map(|span| parse_marker(&span.content)) {
            if !seen.iter().any(|known| known.key == tag.key) {
                seen.push(tag);
            }
        }
    }
    seen
}

/// The previews `frame` shows: per image, the longest run of consecutive
/// reserved rows that kept their marker and blank cells, top to bottom.
pub(crate) fn plan(frame: &[Line]) -> Vec<Visible> {
    let mut runs: Vec<Visible> = Vec::new();
    // The run each row could extend: the previous row's run, if any.
    let mut open: Option<usize> = None;
    for (row, line) in frame.iter().enumerate() {
        let Ok(row) = u16::try_from(row) else {
            break;
        };
        let tag = line
            .iter()
            .find_map(|span| parse_marker(&span.content))
            .filter(|tag| cells_blank(line, tag));
        let Some(tag) = tag else {
            open = None;
            continue;
        };
        let extends = open.and_then(|at| runs.get_mut(at)).filter(|run| {
            run.key == tag.key
                && run.first + run.rows == tag.index
                && u32::from(run.row) + run.rows == u32::from(row)
        });
        if let Some(run) = extends {
            run.rows += 1;
            continue;
        }
        runs.push(Visible {
            key: tag.key,
            row,
            column: u16::try_from(tag.column).unwrap_or(u16::MAX),
            columns: tag.columns,
            first: tag.index,
            rows: 1,
            total: tag.rows,
        });
        open = Some(runs.len() - 1);
    }
    // One band per image: the tallest (the first on a tie).
    let mut best: Vec<Visible> = Vec::new();
    for run in runs {
        match best.iter_mut().find(|kept| kept.key == run.key) {
            Some(kept) if run.rows > kept.rows => *kept = run,
            Some(_) => {}
            None => best.push(run),
        }
    }
    best.sort_by_key(|band| band.row);
    best
}
