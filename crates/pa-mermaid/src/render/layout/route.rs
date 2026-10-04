//! Box drawing and edge routing. Ported from lovely-mermaid 0.3.3 `layout.ts`
//! (Apache-2.0; see `LICENSE-lovely-mermaid`).

use super::super::canvas::{draw_text, draw_text_over_edges, Canvas, BLANK, D, L, R, U};
use super::super::graph::{Edge, Head, LineKind, Shape};
use super::super::labels::{fit_label, MAX_LABEL};
use super::super::width::measured;
use super::super::Role;
use super::{edge_text, half, sat, width_of, PAD};

/// A node's box on the canvas.
#[derive(Debug, Clone, Copy, Default)]
pub(in crate::render) struct Placed {
    pub(in crate::render) x: i64,
    pub(in crate::render) y: i64,
    pub(in crate::render) w: i64,
    pub(in crate::render) h: i64,
    pub(in crate::render) cx: i64,
    pub(in crate::render) cy: i64,
    pub(in crate::render) rank: usize,
}

/// Draw a box with its label rows centred; `mirrored` draws the rows bottom-up for a
/// canvas a `BT` flip will turn over.
pub(in crate::render) fn draw_box(
    canvas: &mut Canvas,
    p: &Placed,
    lines: &[String],
    shape: Shape,
    mirrored: bool,
) {
    let Placed { x, y, w, h, .. } = *p;
    let right = x + w - 1;
    let bottom = y + h - 1;

    // A diamond is a double-line box — the terminal's nod to `A{...}`.
    let corners = match shape {
        Shape::Diamond => ['╔', '╗', '╚', '╝'],
        Shape::Round => ['╭', '╮', '╰', '╯'],
        Shape::Rect => ['┌', '┐', '└', '┘'],
    };
    canvas.set_char(x, y, corners[0], Role::Border);
    canvas.set_char(right, y, corners[1], Role::Border);
    canvas.set_char(x, bottom, corners[2], Role::Border);
    canvas.set_char(right, bottom, corners[3], Role::Border);

    if shape == Shape::Diamond {
        // Double lines carry no direction bits; edges tee into them through the mixed
        // junctions `finalize_mask` resolves.
        for cx in x + 1..right {
            canvas.set_char(cx, y, '═', Role::Border);
            canvas.set_char(cx, bottom, '═', Role::Border);
        }
        for cy in y + 1..bottom {
            canvas.set_char(x, cy, '║', Role::Border);
            canvas.set_char(right, cy, '║', Role::Border);
        }
    } else {
        // The perimeter is drawn as bits so edges can tee into it, but it is the box
        // outline, so it claims `border` rather than `edge`.
        for cx in x + 1..right {
            canvas.add_bits(cx, y, L | R, Role::Border);
            canvas.add_bits(cx, bottom, L | R, Role::Border);
        }
        for cy in y + 1..bottom {
            canvas.add_bits(x, cy, U | D, Role::Border);
            canvas.add_bits(right, cy, U | D, Role::Border);
        }
    }

    for cy in y..=bottom {
        for cx in x..=right {
            canvas.occupy(cx, cy);
        }
    }

    let inner = sat(w, 2 * PAD + 2).max(1);
    let ordered: Box<dyn Iterator<Item = &String>> = if mirrored {
        Box::new(lines.iter().rev())
    } else {
        Box::new(lines.iter())
    };
    for (li, line) in ordered.enumerate() {
        let text = fit_label(line, inner as usize);
        let text_x = x + 1 + PAD + half(sat(inner, width_of(&text)));
        draw_text(canvas, &text, text_x, y + 1 + li as i64, Role::Text);
    }
}

/// One row of a class box: a rule between compartments, or a line of text.
enum ClassRow {
    Rule,
    Text { text: String, center: bool },
}

/// A class or ER box: sections separated by horizontal rules, title centred.
pub(super) fn draw_class_box(
    canvas: &mut Canvas,
    p: &Placed,
    sections: &[Vec<String>],
    mirrored: bool,
) {
    draw_box(canvas, p, &[], Shape::Rect, false);
    let inner = sat(p.w, 2 * PAD + 2).max(1);
    let mut rows: Vec<ClassRow> = Vec::new();
    for (si, section) in sections.iter().enumerate() {
        if section.is_empty() {
            continue;
        }
        if !rows.is_empty() {
            rows.push(ClassRow::Rule);
        }
        for line in section {
            rows.push(ClassRow::Text {
                text: fit_label(line, inner as usize),
                center: si == 0,
            });
        }
    }
    if mirrored {
        rows.reverse();
    }
    for (ri, r) in rows.iter().enumerate() {
        let row = p.y + 1 + ri as i64;
        match r {
            ClassRow::Rule => {
                canvas.set_char(p.x, row, '├', Role::Border);
                for x in p.x + 1..p.x + p.w - 1 {
                    canvas.set_char(x, row, '─', Role::Border);
                }
                canvas.set_char(p.x + p.w - 1, row, '┤', Role::Border);
            }
            ClassRow::Text { text, center } => {
                let tx = if *center {
                    p.x + 1 + PAD + half(sat(inner, width_of(text)))
                } else {
                    p.x + 1 + PAD
                };
                draw_text_over_edges(canvas, text, tx, row, Role::Text);
            }
        }
    }
}

/// A subgraph frame: a titled box with a finished sub-canvas centred inside. An
/// unlabelled frame (a state `--` region) keeps its border unbroken.
pub(super) fn draw_frame(
    canvas: &mut Canvas,
    p: &Placed,
    title: &str,
    sub: &Canvas,
    mirrored: bool,
) {
    draw_box(canvas, p, &[], Shape::Rect, false);
    if !title.is_empty() {
        let t = fit_label(title, sat(p.w, 4) as usize);
        // Mirrored: the bottom border becomes the top after the flip.
        let row = if mirrored { p.y + p.h - 1 } else { p.y };
        draw_text_over_edges(canvas, &format!(" {t} "), p.x + 1, row, Role::Text);
    }
    canvas.blit(
        sub,
        p.x + 1 + half(p.w - 2 - sub.w),
        p.y + 1 + half(p.h - 2 - sub.h),
    );
}

// ------------------------------------------------------------------- routing

fn head_glyph(head: Head, arrow: char) -> char {
    match head {
        Head::Circle => 'o',
        Head::Cross => '×',
        Head::DiamondFill => '◆',
        Head::DiamondOpen => '◇',
        Head::Triangle => match arrow {
            '▼' => '▽',
            '▲' => '△',
            '◄' => '◁',
            '▶' => '▷',
            other => other,
        },
        Head::None | Head::Arrow => arrow,
    }
}

/// Adjacent ranks, top-down: drop, jog along the bus row, drop into the head. `entry_x`
/// overrides the centre entry column when the target's top is shared with skip entries
/// (`-1`: the centre); `label_left` renders the label left of the arrowhead.
pub(super) fn route_forward(
    canvas: &mut Canvas,
    from: &Placed,
    to: &Placed,
    edge: &Edge,
    bus: i64,
    entry_x: i64,
    label_left: bool,
) {
    let tx = if entry_x == -1 { to.cx } else { entry_x };
    // A jog of one column reads as a kink; snap straight instead.
    let bx = if (from.cx - tx).abs() <= 1 {
        tx
    } else {
        from.cx
    };
    let by = from.y + from.h - 1;
    let head_row = to.y - 1;

    canvas.junction(bx, by, D);
    canvas.seg_v(bx, by, bus);
    if bx == tx {
        canvas.seg_v(bx, bus, head_row);
    } else {
        canvas.seg_h(bus, bx, tx);
        canvas.seg_v(tx, bus, head_row);
    }

    if edge.head_to == Head::None {
        canvas.add_bits(tx, head_row, U, Role::Edge);
    } else {
        canvas.set_char(tx, head_row, head_glyph(edge.head_to, '▼'), Role::Edge);
    }
    if edge.head_from != Head::None {
        canvas.set_char(bx, by, head_glyph(edge.head_from, '▲'), Role::Edge);
    }

    if edge.card_from.is_none() && edge.card_to.is_none() {
        if let Some(label) = &edge.label {
            let start = if label_left {
                sat(tx, width_of(label).min(MAX_LABEL as i64))
            } else {
                tx + 1
            };
            place_label(canvas, label, head_row, start);
        }
        return;
    }
    // Cardinalities sit at their own ends; the verb takes the row above the head, falling
    // back beside the target card when the gap has no spare row.
    let src_row = by + 1;
    if let Some(card) = &edge.card_from {
        place_label(canvas, card, src_row, bx + 1);
    }
    if let Some(card) = &edge.card_to {
        place_label(canvas, card, head_row, tx + 1);
    }
    if let Some(label) = &edge.label {
        let mid_row = head_row - 1;
        if mid_row > src_row {
            let line_x = if mid_row > bus { tx } else { bx };
            place_label(canvas, label, mid_row, line_x + 1);
        } else {
            let after_card = edge.card_to.as_deref().map_or(0, |c| width_of(c) + 1);
            place_label(canvas, label, head_row, tx + 1 + after_card);
        }
    }
}

/// Adjacent ranks, top-down, against the flow: up out of the source's top, jog along the
/// band, arrow into the target's bottom — the short local return mermaid draws.
pub(super) fn route_back_adjacent(
    canvas: &mut Canvas,
    from: &Placed,
    to: &Placed,
    edge: &Edge,
    bus: i64,
) {
    // Attach right of centre so the return does not merge with the forward exits and
    // arrivals that own the centre column.
    let tx = (to.x + to.w - 2).min(to.cx + 2);
    let bx0 = (from.x + from.w - 2).min(from.cx + 2);
    let bx = if (bx0 - tx).abs() <= 1 { tx } else { bx0 };
    let fy = from.y;
    let head_row = to.y + to.h;

    canvas.junction(bx, fy, U);
    if bx == tx {
        canvas.seg_v(bx, head_row, fy);
    } else {
        canvas.seg_v(bx, bus, fy);
        canvas.seg_h(bus, bx, tx);
        canvas.seg_v(tx, head_row, bus);
    }

    if edge.head_to == Head::None {
        canvas.add_bits(tx, head_row, D, Role::Edge);
    } else {
        canvas.set_char(tx, head_row, head_glyph(edge.head_to, '▲'), Role::Edge);
    }
    if edge.head_from != Head::None {
        canvas.set_char(bx, fy, head_glyph(edge.head_from, '▼'), Role::Edge);
    }

    if let Some(text) = edge_text(edge) {
        place_label(canvas, &text, head_row, tx + 1);
    }
}

/// A self-edge: a stub loop hanging below the box.
pub(super) fn route_self(canvas: &mut Canvas, p: &Placed, edge: &Edge) {
    let bottom = p.y + p.h - 1;
    let exit_x = p.cx + 1;
    let ret_x = p.x + p.w - 2;
    if ret_x <= exit_x || bottom + 2 >= canvas.h {
        return;
    }

    let [v, h, bl, br] = match edge.line {
        LineKind::Dotted => ['╎', '╌', '╰', '╯'],
        LineKind::Thick => ['┃', '━', '┗', '┛'],
        LineKind::Solid => ['│', '─', '╰', '╯'],
    };

    canvas.junction(exit_x, bottom, D);
    canvas.set_char(exit_x, bottom + 1, v, Role::Edge);
    canvas.set_char(exit_x, bottom + 2, bl, Role::Edge);
    for x in exit_x + 1..ret_x {
        canvas.set_char(x, bottom + 2, h, Role::Edge);
    }
    canvas.set_char(ret_x, bottom + 2, br, Role::Edge);
    canvas.set_char(ret_x, bottom + 1, head_glyph(edge.head_to, '▲'), Role::Edge);
    if let Some(text) = edge_text(edge) {
        place_label(canvas, &text, bottom + 1, p.x + p.w + 1);
    }
}

/// Where a top-down forward skip runs.
pub(super) struct SkipRoute {
    pub(super) lane_x: i64,
    pub(super) entry_x: i64,
    pub(super) bus_y: i64,
    pub(super) approach_y: i64,
    pub(super) straight: bool,
    pub(super) label_left: bool,
}

/// Forward skip edge, top-down: out the source's bottom onto its band's bus row, then
/// straight down an unobstructed column into the target's top, or around: down the lane
/// and in along the reserved approach row above the target's rank.
pub(super) fn route_skip(
    canvas: &mut Canvas,
    from: &Placed,
    to: &Placed,
    edge: &Edge,
    at: &SkipRoute,
) {
    let bx = from.cx;
    let bottom = from.y + from.h - 1;

    canvas.junction(bx, bottom, D);
    canvas.seg_v(bx, bottom, at.bus_y);
    if at.straight {
        canvas.seg_h(at.bus_y, bx, at.entry_x);
        canvas.seg_v(at.entry_x, at.bus_y, to.y - 1);
    } else {
        canvas.seg_h(at.bus_y, bx, at.lane_x);
        canvas.seg_v(at.lane_x, at.bus_y, at.approach_y);
        canvas.seg_h(at.approach_y, at.entry_x, at.lane_x);
        canvas.seg_v(at.entry_x, at.approach_y, to.y - 1);
    }

    if edge.head_to == Head::None {
        canvas.add_bits(at.entry_x, to.y - 1, D, Role::Edge);
    } else {
        canvas.set_char(
            at.entry_x,
            to.y - 1,
            head_glyph(edge.head_to, '▼'),
            Role::Edge,
        );
    }
    if edge.head_from != Head::None {
        canvas.set_char(bx, bottom, head_glyph(edge.head_from, '▲'), Role::Edge);
    }

    if let Some(text) = edge_text(edge) {
        let start = if at.label_left {
            sat(at.entry_x, width_of(&text).min(MAX_LABEL as i64))
        } else {
            at.entry_x + 1
        };
        place_label(canvas, &text, to.y - 1, start);
    }
}

/// Multi-rank back edge, top-down: out the right side, up a lane, back in through the
/// target's right side.
pub(super) fn route_back(
    canvas: &mut Canvas,
    from: &Placed,
    to: &Placed,
    edge: &Edge,
    lane_x: i64,
) {
    let sx = from.x + from.w - 1;
    let sy = from.cy;

    canvas.junction(sx, sy, R);
    canvas.seg_h(sy, sx, lane_x);
    if edge.head_from != Head::None {
        canvas.set_char(sx, sy, head_glyph(edge.head_from, '◄'), Role::Edge);
    }

    let tx = to.x + to.w - 1;
    let tyc = to.cy;
    canvas.seg_v(lane_x, sy, tyc);
    canvas.seg_h(tyc, tx + 1, lane_x);
    if edge.head_to == Head::None {
        canvas.add_bits(tx + 1, tyc, R, Role::Edge);
    } else {
        canvas.set_char(tx + 1, tyc, head_glyph(edge.head_to, '◄'), Role::Edge);
    }
    if let Some(text) = edge_text(edge) {
        place_label(canvas, &text, sat(tyc, 1), sat(lane_x, width_of(&text) + 1));
    }
}

/// Adjacent ranks, left-to-right: out the right side, jog on the bus column.
pub(super) fn route_forward_lr(
    canvas: &mut Canvas,
    from: &Placed,
    to: &Placed,
    edge: &Edge,
    bus: i64,
) {
    let rx = from.x + from.w - 1;
    let ry = from.cy;
    let ly = to.cy;
    let head_col = to.x - 1;

    canvas.junction(rx, ry, R);
    canvas.seg_h(ry, rx, bus);
    if ry == ly {
        canvas.seg_h(ry, bus, head_col);
    } else {
        canvas.seg_v(bus, ry, ly);
        canvas.seg_h(ly, bus, head_col);
    }

    if edge.head_to == Head::None {
        canvas.add_bits(head_col, ly, R, Role::Edge);
    } else {
        canvas.set_char(head_col, ly, head_glyph(edge.head_to, '▶'), Role::Edge);
    }
    if edge.head_from != Head::None {
        canvas.set_char(rx, ry, head_glyph(edge.head_from, '◄'), Role::Edge);
    }

    // The verb keeps its spot above the line; cardinalities hug their own ends.
    if let Some(label) = &edge.label {
        place_label(canvas, label, sat(ly, 1), bus + 1);
    }
    if let Some(card) = &edge.card_from {
        place_label(canvas, card, sat(ry, 1), rx + 1);
    }
    if let Some(card) = &edge.card_to {
        place_label(canvas, card, sat(ly, 1), sat(head_col, width_of(card)));
    }
}

/// Straight forward skip, left-to-right: out the source's right-side fan, jog on its own
/// band's bus column, then straight along the target's entry row into its left side.
pub(super) fn route_skip_lr(
    canvas: &mut Canvas,
    from: &Placed,
    to: &Placed,
    edge: &Edge,
    bus: i64,
) {
    let ry = from.cy;
    let ty = to.cy;
    let rx = from.x + from.w - 1;
    let head_col = to.x - 1;

    canvas.junction(rx, ry, R);
    canvas.seg_h(ry, rx, bus);
    if ry != ty {
        canvas.seg_v(bus, ry, ty);
    }
    canvas.seg_h(ty, bus, head_col);

    if edge.head_to == Head::None {
        canvas.add_bits(head_col, ty, R, Role::Edge);
    } else {
        canvas.set_char(head_col, ty, head_glyph(edge.head_to, '▶'), Role::Edge);
    }
    if edge.head_from != Head::None {
        canvas.set_char(rx, ry, head_glyph(edge.head_from, '◄'), Role::Edge);
    }

    // Label after the bus jog, where forward labels sit.
    if let Some(text) = edge_text(edge) {
        place_label(canvas, &text, sat(ty, 1), bus + 1);
    }
}

/// A lane label waiting for every route to land before claiming its spot.
pub(super) struct LaneLabel {
    text: String,
    y: i64,
    lo: i64,
    hi: i64,
}

/// Skip or back edge, left-to-right: down out the bottom, along a lane, back up.
pub(super) fn route_back_lr(
    canvas: &mut Canvas,
    from: &Placed,
    to: &Placed,
    edge: &Edge,
    lane_y: i64,
    lane_labels: &mut Vec<LaneLabel>,
) {
    let sx = from.cx;
    let sy = from.y + from.h - 1;
    let tx = to.cx;
    let ty = to.y + to.h - 1;

    canvas.junction(sx, sy, D);
    canvas.seg_v(sx, sy, lane_y);
    canvas.seg_h(lane_y, sx, tx);
    canvas.seg_v(tx, lane_y, ty + 1);

    if edge.head_to == Head::None {
        canvas.add_bits(tx, ty + 1, D, Role::Edge);
    } else {
        canvas.set_char(tx, ty + 1, head_glyph(edge.head_to, '▲'), Role::Edge);
    }
    if edge.head_from != Head::None {
        canvas.set_char(sx, sy, head_glyph(edge.head_from, '▲'), Role::Edge);
    }

    // The label interrupts its own lane row; deferred until every edge landed so it can
    // dodge the verticals that cross this row.
    if let Some(text) = edge_text(edge) {
        lane_labels.push(LaneLabel {
            text: format!(" {} ", fit_label(&text, MAX_LABEL)),
            y: lane_y,
            lo: sx.min(tx),
            hi: sx.max(tx),
        });
    }
}

/// Write each lane label onto its own row, centred on the run but slid to the nearest
/// stretch free of crossing verticals, arrowheads, and earlier labels.
pub(super) fn place_lane_labels(canvas: &mut Canvas, labels: &[LaneLabel]) {
    for label in labels {
        let LaneLabel { text, y, lo, hi } = label;
        let (y, lo, hi) = (*y, *lo, *hi);
        let tw = width_of(text);
        let last_start = hi - 1 - tw;
        if last_start < lo + 1 || y >= canvas.h {
            continue;
        }
        let clear = |canvas: &Canvas, start: i64| {
            (start..start + tw).all(|x| match canvas.idx(x, y) {
                Some(i) => {
                    canvas.occupied[i] != 1
                        && canvas.mask[i] & (U | D) == 0
                        && canvas.ch[i] == BLANK
                }
                // A cell outside the backing store reads as JS `undefined`: never blank.
                None => false,
            })
        };
        let mid = (half(lo + hi) - half(tw)).max(lo + 1).min(last_start);
        let mut at = mid;
        let mut d = 0;
        loop {
            let left = mid - d;
            let right = mid + d;
            if left < lo + 1 && right > last_start {
                break;
            }
            if left > lo && clear(canvas, left) {
                at = left;
                break;
            }
            if right <= last_start && clear(canvas, right) {
                at = right;
                break;
            }
            d += 1;
        }
        draw_text_over_edges(canvas, text, at, y, Role::EdgeLabel);
    }
}

/// Write an edge label, stopping at the first cell already occupied.
fn place_label(canvas: &mut Canvas, label: &str, row: i64, start_x: i64) {
    if row >= canvas.h {
        return;
    }
    let text = fit_label(label, MAX_LABEL);
    let mut x = start_x;
    for (c, cw) in measured(&text) {
        if cw == 0 {
            continue;
        }
        let cw = cw as i64;
        if x + cw > canvas.w {
            break;
        }
        // A cell outside the backing store reads as JS `undefined`: never blank.
        let blocked = (0..cw).any(|k| match canvas.idx(x + k, row) {
            Some(i) => canvas.ch[i] != BLANK || canvas.mask[i] != 0 || canvas.occupied[i] != 0,
            None => true,
        });
        if blocked {
            break;
        }
        draw_text(canvas, c, x, row, Role::EdgeLabel);
        x += cw;
    }
}
