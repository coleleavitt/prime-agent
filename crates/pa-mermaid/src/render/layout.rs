//! Graph layout: rank, order, place, route, draw. Ported from lovely-mermaid 0.3.3
//! `layout.ts` (Apache-2.0; see `LICENSE-lovely-mermaid`).
//!
//! Follows the Sugiyama outline: ranks along the flow axis, reordering within ranks to cut
//! crossings, then relaxing cross-axis positions so chains stay straight. Edges between
//! adjacent ranks share bus rows; skips drop straight down a clear column or route around
//! the diagram through lanes, and top-down back edges between adjacent ranks return
//! locally beside their forward edge. `BT` and `RL` reuse the `TD`/`LR` layouts and flip
//! the finished canvas.
//!
//! Integer coordinates are `i64` like the package's JS numbers; the cross-axis relaxation
//! runs in `f64` with the same operation order and JS `Math.round`, so every placement
//! matches byte for byte.

mod group;
mod route;
mod tracks;

pub(super) use group::layout_grouped;
pub(super) use route::{draw_box, Placed};

use std::collections::HashMap;

use super::canvas::{Canvas, STY_DOT, STY_SOLID, STY_THICK};
use super::graph::{Dir, Edge, Graph, LineKind};
use super::labels::{fit_label, wrap_label, MAX_LABEL, MAX_LINES, WRAP_WIDTH};
use super::width::string_width;
use route::LaneLabel;
use tracks::{assign_tracks, TrackSpan};

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

pub(super) fn width_of(s: &str) -> i64 {
    string_width(s) as i64
}

/// A label's columns, capped where labels truncate.
fn label_cols(s: &str) -> i64 {
    width_of(s).min(MAX_LABEL as i64)
}

/// Everything an edge says, joined — the fallback for routes with no per-end placement
/// (lanes, self-loops, returns). Forward routes place the cardinalities at their own ends.
pub(super) fn edge_text(edge: &Edge) -> Option<String> {
    let parts: Vec<&str> = [
        edge.card_from.as_deref(),
        edge.label.as_deref(),
        edge.card_to.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
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
    /// Class / ER compartments, drawn verbatim and separated by rules.
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
    /// Reserved corridor row above each rank for laned skip approaches.
    skip_approach: Vec<i64>,
    /// Skip edges: the column entering the target's top (`-1`: none).
    edge_entry_x: Vec<i64>,
    /// Skip edges whose entry column is box-free: drop straight, no lane.
    edge_straight: Vec<bool>,
    /// Per node: the forward cluster's entry column (`-1`: the centre).
    fwd_entry_x: Vec<i64>,
    /// Per edge: the label sits left of the arrowhead instead of right.
    edge_label_left: Vec<bool>,
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

// ------------------------------------------------------------------- spans

/// An edge between adjacent ranks running against the flow. Top-down layouts route
/// these locally through the band beside their forward siblings; LR/RL keep the lane.
pub(super) fn is_adjacent_back(ranks: &[usize], e: &Edge) -> bool {
    e.from != e.to && ranks[e.from] == ranks[e.to] + 1
}

fn is_skip(ranks: &[usize], e: &Edge) -> bool {
    e.from != e.to && ranks[e.to] > ranks[e.from] + 1
}

fn is_forward(ranks: &[usize], e: &Edge) -> bool {
    e.from != e.to && ranks[e.to] == ranks[e.from] + 1
}

/// Whether two node centres call for a jog (`exact` in LR, a slack of one column in TD).
#[derive(Clone, Copy)]
enum Jog {
    Exact,
    Loose,
}

/// Edges crossing the band between rank `r` and `r + 1` that must jog sideways, so need a
/// bus row; `with_back` admits adjacent back edges.
fn bus_spans(
    edges: &[Edge],
    ranks: &[usize],
    centers: &[i64],
    r: usize,
    jog: Jog,
    with_back: bool,
) -> Vec<TrackSpan> {
    let mut out = Vec::new();
    for (i, e) in edges.iter().enumerate() {
        let (cf, ct) = (centers[e.from], centers[e.to]);
        let jogs = match jog {
            Jog::Exact => cf != ct,
            Jog::Loose => (cf - ct).abs() > 1,
        };
        let lo = ranks[e.from].min(ranks[e.to]);
        let adjacent = if with_back {
            ranks[e.from].abs_diff(ranks[e.to]) == 1
        } else {
            ranks[e.to] == ranks[e.from] + 1
        };
        if e.from != e.to && adjacent && lo == r && jogs {
            out.push(TrackSpan::new(cf.min(ct), cf.max(ct), e, i, false));
        }
    }
    out
}

/// Edges skipping a rank or running backwards; these go around in a lane.
fn lane_spans(
    edges: &[Edge],
    ranks: &[usize],
    placed: &[Placed],
    vertical: bool,
) -> Vec<TrackSpan> {
    let mut out = Vec::new();
    for (i, e) in edges.iter().enumerate() {
        if e.from == e.to || ranks[e.to] == ranks[e.from] + 1 {
            continue;
        }
        if vertical && is_adjacent_back(ranks, e) {
            continue;
        }
        let (pf, pt) = (&placed[e.from], &placed[e.to]);
        let (a, b) = if vertical {
            (pf.cy.min(pt.cy), pf.cy.max(pt.cy))
        } else {
            (pf.cx.min(pt.cx), pf.cx.max(pt.cx))
        };
        out.push(TrackSpan::new(a, b, e, i, edge_text(e).is_some()));
    }
    out
}

// ----------------------------------------------------------------- placement

/// One entry into a node's top: the merged forward cluster or one skip.
struct TopEntry {
    slot: i64,
    label_w: i64,
    skip: Option<usize>,
}

/// The widest label a top entry carries (`-1`: none).
fn entry_label_w(e: &Edge) -> i64 {
    [e.label.as_deref(), e.card_to.as_deref()]
        .into_iter()
        .flatten()
        .map(label_cols)
        .max()
        .unwrap_or(-1)
}

/// Spread a node's top entries — the merged forward cluster plus each skip — across its
/// box top, and decide which labels flip left of their arrowhead.
#[allow(clippy::too_many_arguments)] // The plan's per-edge outputs ride as slices.
fn spread_top_entries(
    graph: &Graph,
    centers: &[i64],
    box_l: impl Fn(usize) -> i64,
    box_r: impl Fn(usize) -> i64,
    skips_into: &[Vec<usize>],
    fwds_into: &[Vec<usize>],
    edge_entry_x: &mut [i64],
    fwd_entry_x: &mut [i64],
    edge_label_left: &mut [bool],
) {
    for t in 0..graph.nodes.len() {
        let skips = &skips_into[t];
        if skips.is_empty() {
            continue;
        }
        let fwds = &fwds_into[t];
        let cx = centers[t];
        let left = box_l(t);
        let right = box_r(t);
        let has_fwd = !fwds.is_empty();
        let aligned = has_fwd
            && fwds
                .iter()
                .any(|&i| (centers[graph.edges[i].from] - cx).abs() <= 1);
        let spread_l = if has_fwd && aligned { cx } else { left };
        let slots = if has_fwd && aligned {
            skips.len() as i64
        } else {
            i64::from(has_fwd) + skips.len() as i64
        };
        let slot = |i: i64| {
            spread_l + js_round(((right - spread_l) * (i + 1)) as f64 / (slots + 1) as f64) as i64
        };
        let mut items: Vec<TopEntry> = Vec::new();
        if has_fwd {
            items.push(TopEntry {
                slot: if aligned { cx } else { slot(0) },
                label_w: fwds
                    .iter()
                    .map(|&i| entry_label_w(&graph.edges[i]))
                    .max()
                    .unwrap_or(i64::MIN),
                skip: None,
            });
        }
        for (j, &si) in skips.iter().enumerate() {
            let j = j as i64;
            let index = if has_fwd && aligned {
                j
            } else {
                j + i64::from(has_fwd)
            };
            items.push(TopEntry {
                slot: slot(index),
                label_w: entry_label_w(&graph.edges[si]),
                skip: Some(si),
            });
        }
        // Walk left to right with a cursor over the free head-row cells: each entry lands
        // at its slot (or past the previous label), its label going right when the next
        // slot leaves room, else left when the cells behind the cursor allow.
        let mut cols: Vec<i64> = Vec::new();
        let mut lefts: Vec<bool> = Vec::new();
        let mut cursor = left;
        let mut fits = true;
        for (i, item) in items.iter().enumerate() {
            let x = item.slot.max(cursor);
            if x > right - 1 {
                fits = false;
                break;
            }
            let next = items.get(i + 1).map_or(i64::MAX, |n| n.slot);
            let w = item.label_w;
            if w >= 0 && x + w + 2 > next && x - cursor >= w {
                lefts.push(true);
                cursor = x + 2;
            } else {
                lefts.push(false);
                cursor = if w >= 0 { x + w + 2 } else { x + 2 };
            }
            cols.push(x);
        }
        if fits {
            for (i, item) in items.iter().enumerate() {
                match item.skip {
                    None => {
                        fwd_entry_x[t] = cols[i];
                        if lefts[i] {
                            for &fi in fwds {
                                edge_label_left[fi] = true;
                            }
                        }
                    }
                    Some(si) => {
                        edge_entry_x[si] = cols[i];
                        edge_label_left[si] = lefts[i];
                    }
                }
            }
            continue;
        }
        // Legacy: forward stays centred; a skip lands past the arrival labels, or left of
        // centre with its own label flipped left.
        let reach = if has_fwd {
            fwds.iter()
                .map(|&i| cx + 1 + entry_label_w(&graph.edges[i]))
                .fold(cx, i64::max)
        } else {
            -1
        };
        for &si in skips {
            let clear = if reach == -1 { cx + 2 } else { reach + 2 };
            let capped = clear.min(right - 1);
            if capped > cx.max(reach) {
                edge_entry_x[si] = capped;
            } else {
                edge_entry_x[si] = (left + 1).max(cx - 2);
                edge_label_left[si] = true;
            }
        }
    }
}

fn place_td(
    ranks: &[usize],
    max_rank: usize,
    by_rank: &[Vec<usize>],
    sizes: &NodeSizes,
    graph: &Graph,
    placed: &mut [Placed],
) -> RoutePlan {
    let edges = &graph.edges;
    let centers = assign_positions(by_rank, &sizes.lay_w, GAP_X, edges, ranks);

    // Top-entry geometry, derivable before placement.
    let box_l = |j: usize| sat(centers[j], half(sizes.box_w[j]));
    let box_r = |j: usize| box_l(j) + sizes.box_w[j] - 1;
    let mut edge_entry_x = vec![-1i64; edges.len()];
    let mut edge_straight = vec![false; edges.len()];
    let mut fwd_entry_x = vec![-1i64; graph.nodes.len()];
    let mut edge_label_left = vec![false; edges.len()];
    let mut skips_into: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
    let mut fwds_into: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
    for (i, e) in edges.iter().enumerate() {
        if is_skip(ranks, e) {
            skips_into[e.to].push(i);
        } else if is_forward(ranks, e) {
            fwds_into[e.to].push(i);
        }
    }
    spread_top_entries(
        graph,
        &centers,
        box_l,
        box_r,
        &skips_into,
        &fwds_into,
        &mut edge_entry_x,
        &mut fwd_entry_x,
        &mut edge_label_left,
    );
    // A skip whose entry column crosses no box on any intermediate rank drops straight.
    for (i, e) in edges.iter().enumerate() {
        if !is_skip(ranks, e) {
            continue;
        }
        let entry_x = edge_entry_x[i];
        edge_straight[i] = (0..graph.nodes.len()).all(|j| {
            ranks[j] <= ranks[e.from]
                || ranks[j] >= ranks[e.to]
                || entry_x < box_l(j)
                || entry_x > box_r(j)
        });
    }

    let mut edge_bus = vec![0i64; edges.len()];
    let mut bus_tracks = vec![0i64; max_rank + 1];
    for (r, track_count) in bus_tracks.iter_mut().enumerate().take(max_rank) {
        let mut spans = bus_spans(edges, ranks, &centers, r, Jog::Loose, true);
        // Skip departures ride the bus tracks of their own band: endpoint sharing folds
        // them onto their siblings' row, one `┴` origin.
        for (i, e) in edges.iter().enumerate() {
            if !is_skip(ranks, e) || ranks[e.from] != r {
                continue;
            }
            let end_x = if edge_straight[i] {
                edge_entry_x[i]
            } else {
                1 << 29
            };
            let cf = centers[e.from];
            spans.push(TrackSpan::new(cf.min(end_x), cf.max(end_x), e, i, false));
        }
        if spans.is_empty() {
            continue;
        }
        // Back-edge arrowheads sit on the first band row, back buses right under it,
        // forward buses below those.
        let (back, fwd): (Vec<TrackSpan>, Vec<TrackSpan>) = spans
            .into_iter()
            .partition(|s| is_adjacent_back(ranks, &edges[s.edge]));
        let base = i64::from(
            edges
                .iter()
                .any(|e| is_adjacent_back(ranks, e) && ranks[e.to] == r),
        );
        let (b_assigned, b_count) = assign_tracks(&back, false);
        for (idx, slot) in b_assigned {
            edge_bus[idx] = base + slot;
        }
        let (f_assigned, f_count) = assign_tracks(&fwd, false);
        for (idx, slot) in f_assigned {
            edge_bus[idx] = base + b_count + slot;
        }
        *track_count = base + b_count + f_count;
    }
    // A laned skip re-enters along a reserved approach row above the target's rank.
    let mut enters_into = vec![false; max_rank + 1];
    for (i, e) in edges.iter().enumerate() {
        if is_skip(ranks, e) && !edge_straight[i] {
            enters_into[ranks[e.to]] = true;
        }
    }
    for r in 0..max_rank {
        if enters_into[r + 1] {
            bus_tracks[r] += 1;
        }
    }

    let rank_h: Vec<i64> = by_rank
        .iter()
        .map(|row| {
            row.iter()
                .map(|&i| sizes.box_h[i] + sizes.extra_h[i])
                .max()
                .unwrap_or(3)
        })
        .collect();
    // Per-end cardinalities want a row each around the verb.
    let has_cards = edges
        .iter()
        .any(|e| e.card_from.is_some() || e.card_to.is_some());
    let gap_y = if has_cards { GAP_Y.max(3) } else { GAP_Y };
    let mut rank_y = vec![0i64; max_rank + 1];
    for r in 1..=max_rank {
        rank_y[r] = rank_y[r - 1] + rank_h[r - 1] + gap_y.max(bus_tracks[r - 1] + 1);
    }
    let canvas_h = rank_y[max_rank] + rank_h[max_rank];
    let band_end: Vec<i64> = (0..=max_rank).map(|r| rank_y[r] + rank_h[r]).collect();

    // Approach rows sit just above each band's arrival-head row.
    let mut skip_approach = vec![-1i64; max_rank + 1];
    for r in 0..max_rank {
        if enters_into[r + 1] {
            skip_approach[r + 1] = rank_y[r + 1] - 2;
        }
    }

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
    for e in edges {
        if e.from == e.to {
            continue;
        }
        if is_forward(ranks, e) {
            for part in [e.label.as_deref(), e.card_to.as_deref()]
                .into_iter()
                .flatten()
            {
                content_w = content_w.max(placed[e.to].cx + 2 + label_cols(part));
            }
            if let Some(card) = &e.card_from {
                content_w = content_w.max(placed[e.from].cx + 2 + width_of(card));
            }
        } else if is_adjacent_back(ranks, e) {
            if let Some(text) = edge_text(e) {
                content_w = content_w.max(placed[e.to].cx + 2 + label_cols(&text));
            }
        } else if let Some(text) = edge_text(e) {
            content_w = content_w.max(diagram_w + label_cols(&text) + 1);
        }
    }

    let mut edge_lane = vec![0i64; edges.len()];
    let lanes: Vec<TrackSpan> = lane_spans(edges, ranks, placed, true)
        .into_iter()
        .filter(|s| !edge_straight[s.edge])
        .collect();
    let mut canvas_w = content_w;
    let mut lane_base = 0;
    if !lanes.is_empty() {
        let (assigned, count) = assign_tracks(&lanes, true);
        for (idx, slot) in assigned {
            edge_lane[idx] = slot;
        }
        canvas_w = content_w + 1 + count;
        lane_base = content_w + 1;
    }

    RoutePlan {
        canvas_w,
        canvas_h,
        band_end,
        edge_bus,
        lane_base,
        edge_lane,
        skip_approach,
        edge_entry_x,
        edge_straight,
        fwd_entry_x,
        edge_label_left,
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
    let edges = &graph.edges;
    let col_w: Vec<i64> = by_rank
        .iter()
        .map(|row| row.iter().map(|&i| sizes.box_w[i]).max().unwrap_or(0))
        .collect();

    let centers = assign_positions(by_rank, &sizes.lay_h, 1, edges, ranks);

    // A back-edge target's bottom-entry `▲` stub sits one row below its box; a straight
    // run through that cell would appear to carry the arrival.
    let mut stub_rows: Vec<i64> = Vec::new();
    for e in edges {
        if e.from == e.to || ranks[e.to] >= ranks[e.from] {
            continue;
        }
        let t = e.to;
        stub_rows.push(sat(centers[t], half(sizes.box_h[t] + sizes.extra_h[t])) + sizes.box_h[t]);
    }
    // A skip whose target entry row crosses no box on any intermediate rank runs straight
    // into the target's left side; the bottom lane is the fallback.
    let mut edge_straight = vec![false; edges.len()];
    for (i, e) in edges.iter().enumerate() {
        if !is_skip(ranks, e) {
            continue;
        }
        let row = centers[e.to];
        edge_straight[i] = !stub_rows.contains(&row)
            && (0..graph.nodes.len()).all(|j| {
                ranks[j] <= ranks[e.from]
                    || ranks[j] >= ranks[e.to]
                    || (centers[j] - row).abs() > half(sizes.box_h[j] + sizes.extra_h[j])
            });
    }

    // Edge labels sit in the gap after their source's column, so each gap sizes to the
    // widest label leaving through it.
    let mut band_label = vec![0i64; max_rank + 1];
    for (i, e) in edges.iter().enumerate() {
        if e.from == e.to {
            continue;
        }
        if ranks[e.to] != ranks[e.from] + 1 && !edge_straight[i] {
            continue;
        }
        let verb = e.label.as_deref().map_or(0, label_cols);
        let cards: i64 = [e.card_from.as_deref(), e.card_to.as_deref()]
            .into_iter()
            .flatten()
            .map(|c| width_of(c) + 1)
            .sum();
        let r = ranks[e.from];
        band_label[r] = band_label[r].max(verb + cards);
    }

    let mut edge_bus = vec![0i64; edges.len()];
    let mut bus_tracks = vec![0i64; max_rank + 1];
    for (r, track_count) in bus_tracks.iter_mut().enumerate().take(max_rank) {
        let mut spans = bus_spans(edges, ranks, &centers, r, Jog::Exact, false);
        // Straight-skip departures ride their own band's bus tracks.
        for (i, e) in edges.iter().enumerate() {
            if !is_skip(ranks, e) || !edge_straight[i] || ranks[e.from] != r {
                continue;
            }
            let (cf, ct) = (centers[e.from], centers[e.to]);
            spans.push(TrackSpan::new(cf.min(ct), cf.max(ct), e, i, false));
        }
        if spans.is_empty() {
            continue;
        }
        let (assigned, count) = assign_tracks(&spans, false);
        for (idx, slot) in assigned {
            edge_bus[idx] = slot;
        }
        *track_count = count;
    }

    let mut rank_x = vec![0i64; max_rank + 1];
    for r in 1..=max_rank {
        let gap = (GAP_X + 1)
            .max(band_label[r - 1] + 3)
            .max(bus_tracks[r - 1] + 1);
        rank_x[r] = rank_x[r - 1] + col_w[r - 1] + gap;
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

    let mut edge_lane = vec![0i64; edges.len()];
    let lanes: Vec<TrackSpan> = lane_spans(edges, ranks, placed, false)
        .into_iter()
        .filter(|s| !edge_straight[s.edge])
        .collect();
    let mut canvas_h = diagram_h;
    let mut lane_base = 0;
    if !lanes.is_empty() {
        let (assigned, count) = assign_tracks(&lanes, true);
        for (idx, slot) in assigned {
            edge_lane[idx] = slot;
        }
        canvas_h = diagram_h + 1 + count;
        lane_base = diagram_h + 1;
    }

    RoutePlan {
        canvas_w,
        canvas_h,
        band_end,
        edge_bus,
        lane_base,
        edge_lane,
        skip_approach: vec![-1; max_rank + 1],
        edge_entry_x: vec![-1; edges.len()],
        edge_straight,
        fwd_entry_x: vec![-1; graph.nodes.len()],
        edge_label_left: vec![false; edges.len()],
    }
}

// -------------------------------------------------------------------- canvas

/// Parallel edges ride the same cells, so every label after the first would be lost:
/// join them onto the first instead (before sizing, so the joined label gets its room).
fn join_parallel_labels(edges: &mut [Edge]) {
    let mut first_of: HashMap<(usize, usize), usize> = HashMap::new();
    for i in 0..edges.len() {
        let (from, to) = (edges[i].from, edges[i].to);
        if from == to {
            continue;
        }
        let Some(&first) = first_of.get(&(from, to)) else {
            first_of.insert((from, to), i);
            continue;
        };
        if let Some(label) = edges[i].label.take() {
            edges[first].label = Some(match edges[first].label.take() {
                None => label,
                Some(head) => format!("{head} / {label}"),
            });
        }
    }
}

/// Rank, place, draw, and route a graph onto a fresh canvas; `None` when the diagram is
/// empty or over the cell cap.
fn layout_canvas(graph: &mut Graph, extras: &[NodeExtra]) -> Option<Canvas> {
    let n = graph.nodes.len();
    if n == 0 {
        return None;
    }
    join_parallel_labels(&mut graph.edges);
    let graph = &*graph;

    let ranks = compute_ranks(graph);
    let max_rank = ranks.iter().copied().max().unwrap_or(0);

    let mut by_rank: Vec<Vec<usize>> = vec![Vec::new(); max_rank + 1];
    for (idx, &rank) in ranks.iter().enumerate() {
        by_rank[rank].push(idx);
    }
    order_ranks(&mut by_rank, &graph.edges, &ranks);

    // Lane edges exit through the rank's trailing side toward the lane strip; their
    // endpoints go last within the rank, or whatever sat beyond them would be cut through.
    let vertical = matches!(graph.dir, Dir::Down | Dir::Up);
    let mut in_lane = vec![false; n];
    for e in &graph.edges {
        if vertical && is_adjacent_back(&ranks, e) {
            continue;
        }
        if e.from != e.to && ranks[e.to] != ranks[e.from] + 1 {
            in_lane[e.from] = true;
            in_lane[e.to] = true;
        }
    }
    for row in &mut by_rank {
        row.sort_by_key(|&i| in_lane[i]);
    }

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
        if let Some(text) = edge_text(e) {
            self_label_w[e.from] = self_label_w[e.from].max(label_cols(&text));
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
    let plan = if vertical {
        place_td(&ranks, max_rank, &by_rank, &sizes, graph, &mut placed)
    } else {
        place_lr(&ranks, max_rank, &by_rank, &sizes, graph, &mut placed)
    };

    if plan.canvas_w * plan.canvas_h > MAX_CANVAS_CELLS {
        return None;
    }

    let mut canvas = Canvas::new(plan.canvas_w, plan.canvas_h);
    // `BT` mirrors the finished canvas; multi-row content draws upside down so the flip
    // restores reading order.
    let mirrored = graph.dir == Dir::Up;
    for (idx, extra) in extras.iter().enumerate() {
        let p = &placed[idx];
        match extra {
            NodeExtra::Frame(sub) => {
                route::draw_frame(&mut canvas, p, &graph.nodes[idx].label, sub, mirrored);
            }
            NodeExtra::Compartments(sections) => {
                route::draw_class_box(&mut canvas, p, sections, mirrored);
            }
            NodeExtra::Plain => {
                draw_box(
                    &mut canvas,
                    p,
                    &wrapped[idx],
                    graph.nodes[idx].shape,
                    mirrored,
                );
            }
        }
    }

    let mut lane_labels: Vec<LaneLabel> = Vec::new();
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
        let adjacent_back = from.rank == to.rank + 1;
        // A back edge crosses the band below its target's rank.
        let bus_rank = if adjacent_back { to.rank } else { from.rank };
        let bus = plan.band_end[bus_rank] + plan.edge_bus[i];
        let lane = plan.lane_base + plan.edge_lane[i];
        if vertical {
            if adjacent {
                route::route_forward(
                    &mut canvas,
                    from,
                    to,
                    edge,
                    bus,
                    plan.fwd_entry_x[edge.to],
                    plan.edge_label_left[i],
                );
            } else if adjacent_back {
                route::route_back_adjacent(&mut canvas, from, to, edge, bus);
            } else if to.rank > from.rank {
                route::route_skip(
                    &mut canvas,
                    from,
                    to,
                    edge,
                    &route::SkipRoute {
                        lane_x: lane,
                        entry_x: plan.edge_entry_x[i],
                        bus_y: bus,
                        approach_y: plan.skip_approach[to.rank],
                        straight: plan.edge_straight[i],
                        label_left: plan.edge_label_left[i],
                    },
                );
            } else {
                route::route_back(&mut canvas, from, to, edge, lane);
            }
        } else if adjacent {
            route::route_forward_lr(&mut canvas, from, to, edge, bus);
        } else if to.rank > from.rank && plan.edge_straight[i] {
            route::route_skip_lr(&mut canvas, from, to, edge, bus);
        } else {
            route::route_back_lr(&mut canvas, from, to, edge, lane, &mut lane_labels);
        }
    }
    route::place_lane_labels(&mut canvas, &lane_labels);

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
pub(super) fn layout_flowchart(graph: &mut Graph) -> Option<Canvas> {
    let extras: Vec<NodeExtra> = graph.nodes.iter().map(|_| NodeExtra::Plain).collect();
    let dir = graph.dir;
    layout_canvas(graph, &extras).map(|canvas| orient(canvas, dir))
}

/// Class and ER diagrams: boxes divided into title / attribute / method rows.
pub(super) fn layout_class(graph: &mut Graph) -> Option<Canvas> {
    let extras: Vec<NodeExtra> = graph
        .nodes
        .iter()
        .map(|node| {
            NodeExtra::Compartments(
                node.sections
                    .clone()
                    .unwrap_or_else(|| vec![vec![node.label.clone()]]),
            )
        })
        .collect();
    let dir = graph.dir;
    layout_canvas(graph, &extras).map(|canvas| orient(canvas, dir))
}
