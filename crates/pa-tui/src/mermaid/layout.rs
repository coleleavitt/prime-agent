//! Graph layout: rank, order, place, route, draw. Ported from grok-mermaid 0.2.3
//! `layout.ts` (Apache-2.0; see `LICENSE-grok-mermaid`).
//!
//! Follows the Sugiyama outline: ranks along the flow axis, reordering within ranks to cut
//! crossings, then relaxing cross-axis positions so chains stay straight. Edges between
//! adjacent ranks share bus rows; everything else routes around the diagram through lanes.
//! `BT` and `RL` reuse the `TD`/`LR` layouts and flip the finished canvas.
//!
//! Integer coordinates are `i64` like the package's JS numbers; the cross-axis relaxation
//! runs in `f64` with the same operation order and JS `Math.round`, so every placement
//! matches byte for byte.

mod group;
mod route;

pub(super) use group::layout_grouped;
pub(super) use route::{draw_box, Placed};

use super::canvas::{Canvas, STY_DOT, STY_SOLID, STY_THICK};
use super::graph::{ClassInfo, Dir, Edge, Graph, LineKind};
use super::labels::{fit_label, wrap_label, MAX_LABEL, MAX_LINES, WRAP_WIDTH};
use super::parse::display_generics;
use super::width::string_width;

/// Cells of padding between a box border and its text.
pub(super) const PAD: i64 = 1;
/// Minimum horizontal / vertical space between boxes.
const GAP_X: i64 = 3;
const GAP_Y: i64 = 2;
/// Refuse to allocate a canvas larger than this many cells.
pub(super) const MAX_CANVAS_CELLS: i64 = 1 << 21;

/// Saturating subtraction (the original's `usize` arithmetic never goes negative).
pub(super) fn sat(a: i64, b: i64) -> i64 {
    (a - b).max(0)
}

/// JS `Math.floor(n / 2)`.
pub(super) fn half(n: i64) -> i64 {
    n.div_euclid(2)
}

/// JS `Math.round`: halves round toward positive infinity.
fn js_round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

fn width_of(s: &str) -> i64 {
    string_width(s) as i64
}

/// Per-node dimensions; `lay_*` include room for self-edge loops and labels.
struct NodeSizes {
    box_w: Vec<i64>,
    box_h: Vec<i64>,
    lay_w: Vec<i64>,
    lay_h: Vec<i64>,
    extra_h: Vec<i64>,
    self_label_w: Vec<i64>,
}

/// What to draw inside a node box.
pub(super) enum NodeExtra {
    Plain,
    /// A subgraph frame around its finished sub-canvas.
    Frame(Canvas),
    /// Class / ER compartments: title, attributes, methods.
    Compartments(Vec<Vec<String>>),
}

struct RoutePlan {
    canvas_w: i64,
    canvas_h: i64,
    /// Coordinate just past each rank's boxes, where its bus rows begin.
    band_end: Vec<i64>,
    /// Bus track offset per edge.
    edge_bus: Vec<i64>,
    /// Coordinate of the first lane track.
    lane_base: i64,
    /// Lane track offset per edge.
    edge_lane: Vec<i64>,
}

// ------------------------------------------------------------------ ranking

/// Longest-path ranking over the graph's DAG; back edges (closing a cycle) are excluded
/// by a DFS colouring pass.
fn compute_ranks(graph: &Graph) -> Vec<usize> {
    let n = graph.nodes.len();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut indeg = vec![0usize; n];
    for e in &graph.edges {
        if e.from != e.to {
            children[e.from].push(e.to);
            indeg[e.to] += 1;
        }
    }

    let mut color = vec![0u8; n];
    let mut dag: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut order: Vec<usize> = Vec::new();

    // Roots first so ranks grow from natural entry points, then any leftovers.
    let roots = (0..n).filter(|&i| indeg[i] == 0);
    for start in roots.chain(0..n) {
        if color[start] == 0 {
            dfs_dag(start, &children, &mut color, &mut dag, &mut order);
        }
    }

    let mut rank = vec![0usize; n];
    for &u in order.iter().rev() {
        for &v in &dag[u] {
            rank[v] = rank[v].max(rank[u] + 1);
        }
    }
    rank
}

/// Iterative DFS recording postorder and skipping edges back into the stack.
fn dfs_dag(
    start: usize,
    children: &[Vec<usize>],
    color: &mut [u8],
    dag: &mut [Vec<usize>],
    order: &mut Vec<usize>,
) {
    let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
    color[start] = 1;
    while let Some(frame) = stack.last_mut() {
        let u = frame.0;
        if frame.1 < children[u].len() {
            let v = children[u][frame.1];
            frame.1 += 1;
            if color[v] == 1 {
                continue; // grey: a back edge, ignore it
            }
            dag[u].push(v);
            if color[v] == 0 {
                color[v] = 1;
                stack.push((v, 0));
            }
        } else {
            color[u] = 2;
            order.push(u);
            stack.pop();
        }
    }
}

/// Forward neighbours (parents, children) over edges that go down the ranks.
fn rank_neighbours(
    n: usize,
    edges: &[Edge],
    ranks: &[usize],
) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
    let mut parents: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for e in edges {
        if e.from != e.to && ranks[e.to] > ranks[e.from] {
            parents[e.to].push(e.from);
            children[e.from].push(e.to);
        }
    }
    (parents, children)
}

/// Reorder nodes within each rank to minimise edge crossings (barycenter sweeps), keeping
/// whichever ordering crossed least.
fn order_ranks(by_rank: &mut Vec<Vec<usize>>, edges: &[Edge], ranks: &[usize]) {
    let n = ranks.len();
    if by_rank.len() < 2 || n < 3 {
        return;
    }
    let (parents, children) = rank_neighbours(n, edges, ranks);

    let mut pos = vec![0usize; n];
    let reindex = |row: &[usize], pos: &mut [usize]| {
        for (i, &v) in row.iter().enumerate() {
            pos[v] = i;
        }
    };
    for row in by_rank.iter() {
        reindex(row, &mut pos);
    }

    let mut best = by_rank.clone();
    let mut best_crossings = count_crossings(edges, ranks, &pos);
    if best_crossings == 0 {
        return;
    }

    for it in 0..8 {
        // Alternate sweeping down (sort by parents) and up (sort by children).
        let (rows, neigh): (Vec<usize>, &[Vec<usize>]) = if it % 2 == 0 {
            ((1..by_rank.len()).collect(), &parents)
        } else {
            ((0..by_rank.len() - 1).rev().collect(), &children)
        };
        for r in rows {
            sort_by_barycenter(&mut by_rank[r], neigh, &pos);
            reindex(&by_rank[r], &mut pos);
        }
        let crossings = count_crossings(edges, ranks, &pos);
        if crossings < best_crossings {
            best_crossings = crossings;
            best.clone_from(by_rank);
        }
        if best_crossings == 0 {
            break;
        }
    }
    *by_rank = best;
}

/// The mean of the neighbours' positions, as JS sums them (left to right from zero).
fn barycenter(neigh: &[usize], own: f64, pos: impl Fn(usize) -> f64) -> f64 {
    if neigh.is_empty() {
        return own;
    }
    let sum = neigh.iter().fold(0.0, |s, &u| s + pos(u));
    sum / neigh.len() as f64
}

fn sort_by_barycenter(row: &mut [usize], neigh: &[Vec<usize>], pos: &[usize]) {
    let mut keyed: Vec<(f64, usize)> = row
        .iter()
        .map(|&v| (barycenter(&neigh[v], pos[v] as f64, |u| pos[u] as f64), v))
        .collect();
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (slot, (_, v)) in row.iter_mut().zip(keyed) {
        *slot = v;
    }
}

fn count_crossings(edges: &[Edge], ranks: &[usize], pos: &[usize]) -> usize {
    let adjacent: Vec<(usize, usize, usize)> = edges
        .iter()
        .filter(|e| e.from != e.to && ranks[e.to] == ranks[e.from] + 1)
        .map(|e| (ranks[e.from], pos[e.from], pos[e.to]))
        .collect();
    let mut crossings = 0;
    for (i, a) in adjacent.iter().enumerate() {
        for b in &adjacent[i + 1..] {
            if a.0 == b.0 && ((a.1 < b.1 && a.2 > b.2) || (a.1 > b.1 && a.2 < b.2)) {
                crossings += 1;
            }
        }
    }
    crossings
}

/// A cross-axis centre for every node: each drifts toward the mean of its neighbours
/// while ranks keep their order and boxes keep `sep` between them.
fn assign_positions(
    by_rank: &[Vec<usize>],
    size: &[i64],
    sep: i64,
    edges: &[Edge],
    ranks: &[usize],
) -> Vec<i64> {
    let n = size.len();
    let (parents, children) = rank_neighbours(n, edges, ranks);
    let sep = sep as f64;

    let mut pos = vec![0.0f64; n];
    for row in by_rank {
        let mut x = 0.0;
        for &v in row {
            let h = size[v] as f64 / 2.0;
            x += h;
            pos[v] = x;
            x += h + sep;
        }
    }

    for it in 0..10 {
        let neigh = if it % 2 == 0 { &parents } else { &children };
        if it % 2 == 0 {
            for row in by_rank {
                relax_rank(row, neigh, &mut pos, size, sep);
            }
        } else {
            for row in by_rank.iter().rev() {
                relax_rank(row, neigh, &mut pos, size, sep);
            }
        }
    }

    let mut min_left = f64::INFINITY;
    for v in 0..n {
        min_left = min_left.min(pos[v] - size[v] as f64 / 2.0);
    }
    if !min_left.is_finite() {
        min_left = 0.0;
    }
    (0..n)
        .map(|v| js_round(pos[v] - min_left).max(0.0) as i64)
        .collect()
}

fn relax_rank(nodes: &[usize], neigh: &[Vec<usize>], pos: &mut [f64], size: &[i64], sep: f64) {
    let n = nodes.len();
    if n == 0 {
        return;
    }
    let desired: Vec<f64> = nodes
        .iter()
        .map(|&v| barycenter(&neigh[v], pos[v], |u| pos[u]))
        .collect();
    let half_of = |i: usize| size[nodes[i]] as f64 / 2.0;

    // Sweep right then left and take the midpoint: this centres a node between the
    // tightest packing that respects order from either side.
    let mut left = vec![0.0f64; n];
    for i in 0..n {
        left[i] = if i == 0 {
            desired[i]
        } else {
            desired[i].max(left[i - 1] + half_of(i - 1) + sep + half_of(i))
        };
    }
    let mut right = vec![0.0f64; n];
    for i in (0..n).rev() {
        right[i] = if i == n - 1 {
            desired[i]
        } else {
            desired[i].min(right[i + 1] - half_of(i + 1) - sep - half_of(i))
        };
    }
    for i in 0..n {
        pos[nodes[i]] = f64::midpoint(left[i], right[i]);
    }
    for i in 1..n {
        let min_p = pos[nodes[i - 1]] + half_of(i - 1) + sep + half_of(i);
        if pos[nodes[i]] < min_p {
            pos[nodes[i]] = min_p;
        }
    }
}

// ------------------------------------------------------------------- tracks

/// A span competing for a track: `[start, end, from, to, edge index]`.
type Span5 = [i64; 5];

/// Pack spans into as few parallel tracks as possible. Two spans share a track when they
/// are two cells apart, or share an endpoint (a fan-out reuses one row). Returns each edge
/// index's track and the track count.
fn assign_tracks(spans: &[Span5]) -> (Vec<(usize, i64)>, i64) {
    let mut sorted = spans.to_vec();
    sorted.sort_unstable();
    let mut tracks: Vec<Vec<[i64; 4]>> = Vec::new();
    let mut assigned: Vec<(usize, i64)> = Vec::new();
    for [s, e, f, t, idx] in sorted {
        let slot = tracks
            .iter()
            .position(|members| {
                members
                    .iter()
                    .all(|&[s2, e2, f2, t2]| e2 + 2 <= s || e + 2 <= s2 || f2 == f || t2 == t)
            })
            .unwrap_or_else(|| {
                tracks.push(Vec::new());
                tracks.len() - 1
            });
        tracks[slot].push([s, e, f, t]);
        assigned.push((idx as usize, slot as i64));
    }
    (assigned, tracks.len() as i64)
}

/// Whether two node centres call for a jog (`exact` in LR, a slack of one column in TD).
#[derive(Clone, Copy)]
enum Jog {
    Exact,
    Loose,
}

/// Edges from rank `r` to `r + 1` that must jog sideways, so need a bus row.
fn bus_spans(edges: &[Edge], ranks: &[usize], centers: &[i64], r: usize, jog: Jog) -> Vec<Span5> {
    let mut out = Vec::new();
    for (i, e) in edges.iter().enumerate() {
        let (cf, ct) = (centers[e.from], centers[e.to]);
        let jogs = match jog {
            Jog::Exact => cf != ct,
            Jog::Loose => (cf - ct).abs() > 1,
        };
        if e.from != e.to && ranks[e.from] == r && ranks[e.to] == r + 1 && jogs {
            out.push([cf.min(ct), cf.max(ct), e.from as i64, e.to as i64, i as i64]);
        }
    }
    out
}

/// The flow axis an edge set is laid along.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    Vertical,
    Horizontal,
}

/// Edges skipping a rank or running backwards; these go around in a lane.
fn lane_spans(edges: &[Edge], ranks: &[usize], placed: &[Placed], axis: Axis) -> Vec<Span5> {
    let mut out = Vec::new();
    for (i, e) in edges.iter().enumerate() {
        if e.from == e.to || ranks[e.to] == ranks[e.from] + 1 {
            continue;
        }
        let (pf, pt) = (&placed[e.from], &placed[e.to]);
        let (a, b) = match axis {
            Axis::Vertical => (pf.cy.min(pt.cy), pf.cy.max(pt.cy)),
            Axis::Horizontal => (pf.cx.min(pt.cx), pf.cx.max(pt.cx)),
        };
        out.push([a, b, e.from as i64, e.to as i64, i as i64]);
    }
    out
}

/// Bus tracks for every rank gap: each edge's track offset and the track count per rank.
fn bus_tracks(
    graph: &Graph,
    ranks: &[usize],
    max_rank: usize,
    centers: &[i64],
    jog: Jog,
) -> (Vec<i64>, Vec<i64>) {
    let mut edge_bus = vec![0i64; graph.edges.len()];
    let mut tracks = vec![0i64; max_rank + 1];
    for (r, track_count) in tracks.iter_mut().enumerate().take(max_rank) {
        let spans = bus_spans(&graph.edges, ranks, centers, r, jog);
        if spans.is_empty() {
            continue;
        }
        let (assigned, count) = assign_tracks(&spans);
        for (idx, slot) in assigned {
            edge_bus[idx] = slot;
        }
        *track_count = count;
    }
    (edge_bus, tracks)
}

/// Lane tracks for skip and back edges: each edge's track offset and the track count.
fn lane_tracks(graph: &Graph, ranks: &[usize], placed: &[Placed], axis: Axis) -> (Vec<i64>, i64) {
    let mut edge_lane = vec![0i64; graph.edges.len()];
    let lanes = lane_spans(&graph.edges, ranks, placed, axis);
    if lanes.is_empty() {
        return (edge_lane, 0);
    }
    let (assigned, count) = assign_tracks(&lanes);
    for (idx, slot) in assigned {
        edge_lane[idx] = slot;
    }
    (edge_lane, count)
}

// ----------------------------------------------------------------- placement

fn place_td(
    ranks: &[usize],
    max_rank: usize,
    by_rank: &[Vec<usize>],
    sizes: &NodeSizes,
    graph: &Graph,
    placed: &mut [Placed],
) -> RoutePlan {
    let centers = assign_positions(by_rank, &sizes.lay_w, GAP_X, &graph.edges, ranks);
    let (edge_bus, bus_tracks) = bus_tracks(graph, ranks, max_rank, &centers, Jog::Loose);

    let rank_h: Vec<i64> = by_rank
        .iter()
        .map(|row| {
            row.iter()
                .map(|&i| sizes.box_h[i] + sizes.extra_h[i])
                .max()
                .unwrap_or(3)
        })
        .collect();
    let mut rank_y = vec![0i64; max_rank + 1];
    for r in 1..=max_rank {
        rank_y[r] = rank_y[r - 1] + rank_h[r - 1] + GAP_Y.max(bus_tracks[r - 1] + 1);
    }
    let canvas_h = rank_y[max_rank] + rank_h[max_rank];
    let band_end: Vec<i64> = (0..=max_rank).map(|r| rank_y[r] + rank_h[r]).collect();

    let mut diagram_w = 1;
    for (r, row) in by_rank.iter().enumerate() {
        for &idx in row {
            let w = sizes.box_w[idx];
            let h = sizes.box_h[idx];
            let cx = centers[idx];
            let x = sat(cx, half(w));
            let y = rank_y[r] + half(rank_h[r] - h - sizes.extra_h[idx]);
            placed[idx] = Placed {
                x,
                y,
                w,
                h,
                cx,
                cy: y + half(h),
                rank: r,
            };
            diagram_w = diagram_w.max(x + w);
            if sizes.extra_h[idx] > 0 && sizes.self_label_w[idx] > 0 {
                diagram_w = diagram_w.max(x + w + 2 + sizes.self_label_w[idx]);
            }
        }
    }

    let mut content_w = diagram_w;
    for e in &graph.edges {
        let Some(label) = &e.label else {
            continue;
        };
        if e.from == e.to {
            continue;
        }
        let lw = width_of(label).min(MAX_LABEL as i64);
        content_w = if ranks[e.to] == ranks[e.from] + 1 {
            content_w.max(placed[e.to].cx + 2 + lw)
        } else {
            content_w.max(diagram_w + lw + 1)
        };
    }

    let (edge_lane, lane_count) = lane_tracks(graph, ranks, placed, Axis::Vertical);
    let (canvas_w, lane_base) = if lane_count > 0 {
        (content_w + 1 + lane_count, content_w + 1)
    } else {
        (content_w, 0)
    };

    RoutePlan {
        canvas_w,
        canvas_h,
        band_end,
        edge_bus,
        lane_base,
        edge_lane,
    }
}

fn place_lr(
    ranks: &[usize],
    max_rank: usize,
    by_rank: &[Vec<usize>],
    sizes: &NodeSizes,
    graph: &Graph,
    placed: &mut [Placed],
) -> RoutePlan {
    let col_w: Vec<i64> = by_rank
        .iter()
        .map(|row| row.iter().map(|&i| sizes.box_w[i]).max().unwrap_or(0))
        .collect();

    // Left-to-right edge labels sit in the gap between columns, so the gap has to be wide
    // enough for the widest of them.
    let max_label = graph
        .edges
        .iter()
        .filter(|e| e.from == e.to || ranks[e.to] == ranks[e.from] + 1)
        .filter_map(|e| e.label.as_deref())
        .map(|label| width_of(label).min(MAX_LABEL as i64))
        .max()
        .unwrap_or(0);
    let base_gap = (GAP_X + 1).max(max_label + 3);

    let centers = assign_positions(by_rank, &sizes.lay_h, 1, &graph.edges, ranks);
    let (edge_bus, bus_tracks) = bus_tracks(graph, ranks, max_rank, &centers, Jog::Exact);

    let mut rank_x = vec![0i64; max_rank + 1];
    for r in 1..=max_rank {
        rank_x[r] = rank_x[r - 1] + col_w[r - 1] + base_gap.max(bus_tracks[r - 1] + 1);
    }
    let self_tail = by_rank[max_rank]
        .iter()
        .filter(|&&i| sizes.extra_h[i] > 0 && sizes.self_label_w[i] > 0)
        .map(|&i| 2 + sizes.self_label_w[i])
        .max()
        .unwrap_or(0);
    let canvas_w = rank_x[max_rank] + col_w[max_rank] + self_tail;
    let band_end: Vec<i64> = (0..=max_rank).map(|r| rank_x[r] + col_w[r]).collect();

    let mut diagram_h = 1;
    for (r, row) in by_rank.iter().enumerate() {
        let x = rank_x[r];
        for &idx in row {
            let w = sizes.box_w[idx];
            let h = sizes.box_h[idx];
            let cy = centers[idx];
            let y = sat(cy, half(h + sizes.extra_h[idx]));
            placed[idx] = Placed {
                x,
                y,
                w,
                h,
                cx: x + half(w),
                cy: y + half(h),
                rank: r,
            };
            diagram_h = diagram_h.max(y + h + sizes.extra_h[idx]);
        }
    }

    let (edge_lane, lane_count) = lane_tracks(graph, ranks, placed, Axis::Horizontal);
    let (canvas_h, lane_base) = if lane_count > 0 {
        (diagram_h + 1 + lane_count, diagram_h + 1)
    } else {
        (diagram_h, 0)
    };

    RoutePlan {
        canvas_w,
        canvas_h,
        band_end,
        edge_bus,
        lane_base,
        edge_lane,
    }
}

// -------------------------------------------------------------------- canvas

/// Rank, place, draw, and route a graph onto a fresh canvas; `None` when the diagram is
/// empty or over the cell cap.
fn layout_canvas(graph: &Graph, extras: &[NodeExtra]) -> Option<Canvas> {
    let n = graph.nodes.len();
    if n == 0 {
        return None;
    }

    let ranks = compute_ranks(graph);
    let max_rank = ranks.iter().copied().max().unwrap_or(0);

    let mut by_rank: Vec<Vec<usize>> = vec![Vec::new(); max_rank + 1];
    for (idx, &rank) in ranks.iter().enumerate() {
        by_rank[rank].push(idx);
    }
    order_ranks(&mut by_rank, &graph.edges, &ranks);

    let wrapped: Vec<Vec<String>> = graph
        .nodes
        .iter()
        .map(|node| wrap_label(&node.label, WRAP_WIDTH, MAX_LINES))
        .collect();
    let widest = |lines: &mut dyn Iterator<Item = &String>| {
        lines.map(|l| width_of(l)).max().unwrap_or(1).max(1)
    };

    let mut box_w: Vec<i64> = extras
        .iter()
        .enumerate()
        .map(|(i, extra)| match extra {
            NodeExtra::Frame(sub) => {
                (sub.w + 2).max(width_of(&fit_label(&graph.nodes[i].label, WRAP_WIDTH)) + 4)
            }
            NodeExtra::Compartments(sections) => {
                widest(&mut sections.iter().flatten()) + 2 * PAD + 2
            }
            NodeExtra::Plain => widest(&mut wrapped[i].iter()) + 2 * PAD + 2,
        })
        .collect();
    let box_h: Vec<i64> = extras
        .iter()
        .enumerate()
        .map(|(i, extra)| match extra {
            NodeExtra::Frame(sub) => sub.h + 2,
            NodeExtra::Compartments(sections) => {
                let filled = sections.iter().filter(|s| !s.is_empty()).count() as i64;
                let rows: i64 = sections.iter().map(|s| s.len() as i64).sum();
                rows + sat(filled, 1) + 2
            }
            NodeExtra::Plain => wrapped[i].len() as i64 + 2,
        })
        .collect();

    // A self-edge needs two rows below its box, and room beside it for a label.
    let mut extra_h = vec![0i64; n];
    let mut self_label_w = vec![0i64; n];
    for e in &graph.edges {
        if e.from != e.to {
            continue;
        }
        extra_h[e.from] = 2;
        if let Some(label) = &e.label {
            self_label_w[e.from] = self_label_w[e.from].max(width_of(label).min(MAX_LABEL as i64));
        }
    }
    for i in 0..n {
        if extra_h[i] > 0 {
            box_w[i] = box_w[i].max(7);
        }
    }

    let sizes = NodeSizes {
        lay_w: (0..n)
            .map(|i| {
                box_w[i]
                    + if self_label_w[i] > 0 {
                        2 * (self_label_w[i] + 3)
                    } else {
                        0
                    }
            })
            .collect(),
        lay_h: (0..n).map(|i| box_h[i] + extra_h[i]).collect(),
        box_w,
        box_h,
        extra_h,
        self_label_w,
    };

    let mut placed = vec![Placed::default(); n];
    let axis = match graph.dir {
        Dir::Down | Dir::Up => Axis::Vertical,
        Dir::Right | Dir::Left => Axis::Horizontal,
    };
    let plan = match axis {
        Axis::Vertical => place_td(&ranks, max_rank, &by_rank, &sizes, graph, &mut placed),
        Axis::Horizontal => place_lr(&ranks, max_rank, &by_rank, &sizes, graph, &mut placed),
    };

    if plan.canvas_w * plan.canvas_h > MAX_CANVAS_CELLS {
        return None;
    }

    let mut canvas = Canvas::new(plan.canvas_w, plan.canvas_h);
    for (idx, extra) in extras.iter().enumerate() {
        match extra {
            NodeExtra::Frame(sub) => {
                route::draw_frame(&mut canvas, &placed[idx], &graph.nodes[idx].label, sub);
            }
            NodeExtra::Compartments(sections) => {
                route::draw_class_box(&mut canvas, &placed[idx], sections);
            }
            NodeExtra::Plain => {
                draw_box(
                    &mut canvas,
                    &placed[idx],
                    &wrapped[idx],
                    graph.nodes[idx].shape,
                );
            }
        }
    }

    for (i, edge) in graph.edges.iter().enumerate() {
        canvas.cur_style = match edge.line {
            LineKind::Dotted => STY_DOT,
            LineKind::Thick => STY_THICK,
            LineKind::Solid => STY_SOLID,
        };
        if edge.from == edge.to {
            route::route_self(&mut canvas, &placed[edge.from], edge);
            continue;
        }
        let from = &placed[edge.from];
        let to = &placed[edge.to];
        let adjacent = to.rank == from.rank + 1;
        let bus = plan.band_end[from.rank] + plan.edge_bus[i];
        let lane = plan.lane_base + plan.edge_lane[i];
        match (axis, adjacent) {
            (Axis::Vertical, true) => route::route_forward(&mut canvas, from, to, edge, bus),
            (Axis::Vertical, false) => route::route_back(&mut canvas, from, to, edge, lane),
            (Axis::Horizontal, true) => route::route_forward_lr(&mut canvas, from, to, edge, bus),
            (Axis::Horizontal, false) => route::route_back_lr(&mut canvas, from, to, edge, lane),
        }
    }

    canvas.finalize_mask();
    Some(canvas)
}

/// Apply the direction flip a finished canvas needs for `BT` / `RL`.
fn orient(mut canvas: Canvas, dir: Dir) -> Canvas {
    match dir {
        Dir::Up => canvas.flip_vertical(),
        Dir::Left => canvas.flip_horizontal(),
        Dir::Down | Dir::Right => {}
    }
    canvas
}

/// Flowchart and state diagrams: plain boxes, no extra content.
pub(super) fn layout_flowchart(graph: &Graph) -> Option<Canvas> {
    let extras: Vec<NodeExtra> = graph.nodes.iter().map(|_| NodeExtra::Plain).collect();
    layout_canvas(graph, &extras).map(|canvas| orient(canvas, graph.dir))
}

/// Class and ER diagrams: boxes divided into title / attribute / method rows.
pub(super) fn layout_class(graph: &Graph, infos: &[ClassInfo]) -> Option<Canvas> {
    let extras: Vec<NodeExtra> = graph
        .nodes
        .iter()
        .zip(infos)
        .map(|(node, info)| {
            let mut title = Vec::new();
            if let Some(annotation) = &info.annotation {
                title.push(format!("«{annotation}»"));
            }
            title.push(display_generics(&node.label));
            NodeExtra::Compartments(vec![title, info.attrs.clone(), info.methods.clone()])
        })
        .collect();
    layout_canvas(graph, &extras).map(|canvas| orient(canvas, graph.dir))
}
