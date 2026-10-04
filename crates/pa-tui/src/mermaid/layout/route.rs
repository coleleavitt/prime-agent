//! Box drawing and edge routing. Ported from grok-mermaid 0.2.3 `layout.ts`
//! (Apache-2.0; see `LICENSE-grok-mermaid`).

use super::super::canvas::{draw_text, draw_text_over_edges, Canvas, BLANK, D, L, R, U};
use super::super::graph::{Edge, Head, LineKind, Shape};
use super::super::labels::{fit_label, MAX_LABEL};
use super::super::width::measured;
use super::super::Cls;
use super::{half, sat, width_of, PAD};

/// A node's box on the canvas.
#[derive(Debug, Clone, Copy, Default)]
pub(in crate::mermaid) struct Placed {
    pub(in crate::mermaid) x: i64,
    pub(in crate::mermaid) y: i64,
    pub(in crate::mermaid) w: i64,
    pub(in crate::mermaid) h: i64,
    pub(in crate::mermaid) cx: i64,
    pub(in crate::mermaid) cy: i64,
    pub(in crate::mermaid) rank: usize,
}

pub(in crate::mermaid) fn draw_box(
    canvas: &mut Canvas,
    p: &Placed,
    lines: &[String],
    shape: Shape,
) {
    let Placed { x, y, w, h, .. } = *p;
    let right = x + w - 1;
    let bottom = y + h - 1;

    let rounded = matches!(shape, Shape::Round | Shape::Diamond);
    let corners = if rounded {
        ['╭', '╮', '╰', '╯']
    } else {
        ['┌', '┐', '└', '┘']
    };
    canvas.set_char(x, y, corners[0], Cls::Border);
    canvas.set_char(right, y, corners[1], Cls::Border);
    canvas.set_char(x, bottom, corners[2], Cls::Border);
    canvas.set_char(right, bottom, corners[3], Cls::Border);

    // The perimeter is drawn as bits so edges can tee into it, but it is the box outline,
    // so it claims `border` rather than `edge`.
    for cx in x + 1..right {
        canvas.add_bits(cx, y, L | R, Cls::Border);
        canvas.add_bits(cx, bottom, L | R, Cls::Border);
    }
    for cy in y + 1..bottom {
        canvas.add_bits(x, cy, U | D, Cls::Border);
        canvas.add_bits(right, cy, U | D, Cls::Border);
    }

    for cy in y..=bottom {
        for cx in x..=right {
            canvas.occupy(cx, cy);
        }
    }

    let inner = sat(w, 2 * PAD + 2).max(1);
    for (li, line) in lines.iter().enumerate() {
        let text = fit_label(line, inner as usize);
        let text_x = x + 1 + PAD + half(sat(inner, width_of(&text)));
        draw_text(canvas, &text, text_x, y + 1 + li as i64, Cls::Text);
    }
}

/// A class or ER box: sections separated by horizontal rules, title centred.
pub(super) fn draw_class_box(canvas: &mut Canvas, p: &Placed, sections: &[Vec<String>]) {
    draw_box(canvas, p, &[], Shape::Rect);
    let inner = sat(p.w, 2 * PAD + 2).max(1);
    let mut row = p.y + 1;
    let mut first = true;
    for (si, section) in sections.iter().enumerate() {
        if section.is_empty() {
            continue;
        }
        if !first {
            canvas.set_char(p.x, row, '├', Cls::Border);
            for x in p.x + 1..p.x + p.w - 1 {
                canvas.set_char(x, row, '─', Cls::Border);
            }
            canvas.set_char(p.x + p.w - 1, row, '┤', Cls::Border);
            row += 1;
        }
        first = false;
        for line in section {
            let text = fit_label(line, inner as usize);
            let tx = if si == 0 {
                p.x + 1 + PAD + half(sat(inner, width_of(&text)))
            } else {
                p.x + 1 + PAD
            };
            draw_text_over_edges(canvas, &text, tx, row, Cls::Text);
            row += 1;
        }
    }
}

/// A subgraph frame: a titled box with a finished sub-canvas centred inside.
pub(super) fn draw_frame(canvas: &mut Canvas, p: &Placed, title: &str, sub: &Canvas) {
    draw_box(canvas, p, &[], Shape::Rect);
    let t = fit_label(title, sat(p.w, 4) as usize);
    draw_text_over_edges(canvas, &format!(" {t} "), p.x + 1, p.y, Cls::Text);
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

/// Adjacent ranks, top-down: drop, jog along the bus row, drop into the head.
pub(super) fn route_forward(
    canvas: &mut Canvas,
    from: &Placed,
    to: &Placed,
    edge: &Edge,
    bus: i64,
) {
    let tx = to.cx;
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
        canvas.add_bits(tx, head_row, U, Cls::Edge);
    } else {
        canvas.set_char(tx, head_row, head_glyph(edge.head_to, '▼'), Cls::Edge);
    }
    if edge.head_from != Head::None {
        canvas.set_char(bx, by, head_glyph(edge.head_from, '▲'), Cls::Edge);
    }

    if let Some(label) = &edge.label {
        place_label(canvas, label, head_row, tx + 1);
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
    canvas.set_char(exit_x, bottom + 1, v, Cls::Edge);
    canvas.set_char(exit_x, bottom + 2, bl, Cls::Edge);
    for x in exit_x + 1..ret_x {
        canvas.set_char(x, bottom + 2, h, Cls::Edge);
    }
    canvas.set_char(ret_x, bottom + 2, br, Cls::Edge);
    canvas.set_char(ret_x, bottom + 1, head_glyph(edge.head_to, '▲'), Cls::Edge);
    if let Some(label) = &edge.label {
        place_label(canvas, label, bottom + 1, p.x + p.w + 1);
    }
}

/// Skip or back edge, top-down: out the right side, up a lane, back in.
pub(super) fn route_back(
    canvas: &mut Canvas,
    from: &Placed,
    to: &Placed,
    edge: &Edge,
    lane_x: i64,
) {
    let sx = from.x + from.w - 1;
    let sy = from.cy;
    let tx = to.x + to.w - 1;
    let tyc = to.cy;

    canvas.junction(sx, sy, R);
    canvas.seg_h(sy, sx, lane_x);
    canvas.seg_v(lane_x, sy, tyc);
    canvas.seg_h(tyc, tx + 1, lane_x);

    if edge.head_to == Head::None {
        canvas.add_bits(tx + 1, tyc, R, Cls::Edge);
    } else {
        canvas.set_char(tx + 1, tyc, head_glyph(edge.head_to, '◄'), Cls::Edge);
    }
    if edge.head_from != Head::None {
        canvas.set_char(sx, sy, head_glyph(edge.head_from, '◄'), Cls::Edge);
    }

    if let Some(label) = &edge.label {
        place_label(canvas, label, sat(tyc, 1), sat(lane_x, width_of(label) + 1));
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
        canvas.add_bits(head_col, ly, R, Cls::Edge);
    } else {
        canvas.set_char(head_col, ly, head_glyph(edge.head_to, '▶'), Cls::Edge);
    }
    if edge.head_from != Head::None {
        canvas.set_char(rx, ry, head_glyph(edge.head_from, '◄'), Cls::Edge);
    }

    if let Some(label) = &edge.label {
        place_label(canvas, label, sat(ly, 1), bus + 1);
    }
}

/// Skip or back edge, left-to-right: down out the bottom, along a lane, back up.
pub(super) fn route_back_lr(
    canvas: &mut Canvas,
    from: &Placed,
    to: &Placed,
    edge: &Edge,
    lane_y: i64,
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
        canvas.add_bits(tx, ty + 1, D, Cls::Edge);
    } else {
        canvas.set_char(tx, ty + 1, head_glyph(edge.head_to, '▲'), Cls::Edge);
    }
    if edge.head_from != Head::None {
        canvas.set_char(sx, sy, head_glyph(edge.head_from, '▲'), Cls::Edge);
    }

    if let Some(label) = &edge.label {
        place_label(canvas, label, sat(lane_y, 1), half(sx + tx));
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
        draw_text(canvas, c, x, row, Cls::EdgeLabel);
        x += cw;
    }
}
