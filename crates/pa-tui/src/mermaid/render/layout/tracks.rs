//! Track packing for bus rows and lanes. Ported from lovely-mermaid 0.3.3 `layout.ts`
//! (Apache-2.0; see `LICENSE-lovely-mermaid`).

use super::super::graph::Edge;

/// A span competing for a track: the covered coordinate range plus its edge.
#[derive(Debug, Clone, Copy)]
pub(super) struct TrackSpan {
    pub(super) start: i64,
    pub(super) end: i64,
    pub(super) from: usize,
    pub(super) to: usize,
    pub(super) edge: usize,
    /// A labelled lane refuses endpoint sharing: the label would appear to cover every
    /// edge merged onto the row.
    pub(super) labeled: bool,
}

impl TrackSpan {
    pub(super) fn new(start: i64, end: i64, e: &Edge, edge: usize, labeled: bool) -> Self {
        Self {
            start,
            end,
            from: e.from,
            to: e.to,
            edge,
            labeled,
        }
    }
}

/// Pack spans into as few parallel tracks as possible. Two spans share a track when they
/// are two cells apart, or share an endpoint (a fan reuses one row, so a merge draws one
/// arrowhead). Lanes pack shortest-first (`shortest_first`): a span contained in another
/// takes the inner track. Returns each edge index's track and the track count.
pub(super) fn assign_tracks(spans: &[TrackSpan], shortest_first: bool) -> (Vec<(usize, i64)>, i64) {
    let mut sorted = spans.to_vec();
    sorted.sort_by(|a, b| {
        let len = if shortest_first {
            (a.end - a.start).cmp(&(b.end - b.start))
        } else {
            std::cmp::Ordering::Equal
        };
        len.then(a.start.cmp(&b.start))
            .then(a.end.cmp(&b.end))
            .then(a.from.cmp(&b.from))
            .then(a.to.cmp(&b.to))
            .then(a.edge.cmp(&b.edge))
    });
    let mut tracks: Vec<Vec<TrackSpan>> = Vec::new();
    let mut assigned: Vec<(usize, i64)> = Vec::new();
    for span in sorted {
        let slot = tracks
            .iter()
            .position(|members| {
                members.iter().all(|m| {
                    m.end + 2 <= span.start
                        || span.end + 2 <= m.start
                        || ((m.from == span.from || m.to == span.to) && !m.labeled && !span.labeled)
                })
            })
            .unwrap_or_else(|| {
                tracks.push(Vec::new());
                tracks.len() - 1
            });
        tracks[slot].push(span);
        assigned.push((span.edge, slot as i64));
    }
    (assigned, tracks.len() as i64)
}
