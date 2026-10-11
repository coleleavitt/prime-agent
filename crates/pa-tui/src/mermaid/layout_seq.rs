//! Sequence diagram layout: one column per participant, lifelines the full height, boxes
//! repeated top and bottom, and column gaps solved from the widest thing between any two
//! columns. Ported from grok-mermaid 0.2.3 `layout-seq.ts` (Apache-2.0; see
//! `LICENSE-grok-mermaid`).

use super::Cls;
use super::canvas::{Canvas, D, L, R, U, draw_text_over_edges};
use super::graph::Shape;
use super::labels::{WRAP_WIDTH, fit_label};
use super::layout::{MAX_CANVAS_CELLS, PAD, Placed, draw_box, half, sat};
use super::parse::{NoteAnchor, SeqHead, SeqItem, Sequence};
use super::width::string_width;

/// Minimum columns between adjacent lifelines.
const SEQ_GAP: i64 = 5;
const BOX_H: i64 = 3;

fn width_of(s: &str) -> i64 {
    string_width(s) as i64
}

/// JS `Math.ceil(n / 2)` for the non-negative widths it is applied to.
fn ceil_half(n: i64) -> i64 {
    (n + 1).div_euclid(2)
}

/// Where a note box sits, given the lifeline positions: `(x, w)`.
fn note_geometry(xs: &[i64], anchor: NoteAnchor, text_w: i64) -> (i64, i64) {
    match anchor {
        NoteAnchor::Over { from, to } => {
            let center = half(xs[from] + xs[to]);
            let w = (xs[to] - xs[from] + 5).max(text_w + 2 * PAD + 2);
            (sat(center, half(w)), w)
        }
        NoteAnchor::Left { at } => {
            let w = text_w + 2 * PAD + 2;
            (sat(xs[at], 2 + w - 1), w)
        }
        NoteAnchor::Right { at } => (xs[at] + 2, text_w + 2 * PAD + 2),
    }
}

fn item_text_w(text: Option<&String>) -> i64 {
    text.map_or(0, |t| width_of(t))
}

pub(super) fn layout_sequence(seq: &Sequence) -> Option<Canvas> {
    let n = seq.labels.len();
    let labels: Vec<String> = seq
        .labels
        .iter()
        .map(|l| fit_label(l, WRAP_WIDTH))
        .collect();
    let box_w: Vec<i64> = labels
        .iter()
        .map(|l| width_of(l).max(1) + 2 * PAD + 2)
        .collect();

    let mut gaps: Vec<i64> = (0..n.saturating_sub(1))
        .map(|i| SEQ_GAP.max(ceil_half(box_w[i]) + ceil_half(box_w[i + 1]) + 1))
        .collect();

    // Each requirement is "columns l..r together need at least `need` cells".
    let mut reqs: Vec<(usize, usize, i64)> = Vec::new();
    for item in &seq.items {
        match item {
            SeqItem::Message { from, to, text, .. } => {
                let tw = item_text_w(text.as_ref());
                if from != to {
                    reqs.push((*from.min(to), *from.max(to), (tw + 2).max(4)));
                } else if from + 1 < n {
                    reqs.push((*from, from + 1, 5 + tw + 2));
                }
            }
            SeqItem::Note { anchor, text } => {
                let tw = width_of(text);
                match *anchor {
                    NoteAnchor::Over { from, to } if from < to => reqs.push((from, to, sat(tw, 1))),
                    NoteAnchor::Over { from, .. } => {
                        let need = ceil_half(tw + 4) + 2;
                        if from > 0 {
                            reqs.push((from - 1, from, need));
                        }
                        if from + 1 < n {
                            reqs.push((from, from + 1, need));
                        }
                    }
                    NoteAnchor::Left { at } if at > 0 => reqs.push((at - 1, at, tw + 7)),
                    NoteAnchor::Right { at } if at + 1 < n => reqs.push((at, at + 1, tw + 7)),
                    NoteAnchor::Left { .. } | NoteAnchor::Right { .. } => {}
                }
            }
            SeqItem::Divider { .. } => {}
        }
    }
    // Narrowest spans first, so a wide requirement absorbs what they already gave.
    reqs.sort_by_key(|&(l, r, _)| r - l);
    for (l, r, need) in reqs {
        let cur: i64 = gaps[l..r].iter().sum();
        if cur < need {
            gaps[r - 1] += need - cur;
        }
    }

    let mut xs = vec![0i64; n];
    xs[0] = half(box_w[0]);
    for i in 1..n {
        xs[i] = xs[i - 1] + gaps[i - 1];
    }

    let mut canvas_w = xs[n - 1] + ceil_half(box_w[n - 1]) + 1;
    for item in &seq.items {
        match item {
            SeqItem::Message { from, to, text, .. } if from == to => {
                canvas_w = canvas_w.max(xs[*from] + 5 + item_text_w(text.as_ref()) + 1);
            }
            SeqItem::Note { anchor, text } => {
                let (x, w) = note_geometry(&xs, *anchor, width_of(text));
                canvas_w = canvas_w.max(x + w + 1);
            }
            SeqItem::Divider { text } => canvas_w = canvas_w.max(width_of(text) + 4),
            SeqItem::Message { .. } => {}
        }
    }

    let mut rows: Vec<i64> = Vec::with_capacity(seq.items.len());
    let mut y = BOX_H + 1;
    for item in &seq.items {
        rows.push(y);
        y += row_height(item);
    }
    let bottom_top = y;
    let canvas_h = bottom_top + BOX_H;

    if canvas_w * canvas_h > MAX_CANVAS_CELLS {
        return None;
    }

    let mut canvas = Canvas::new(canvas_w, canvas_h);

    for i in 0..n {
        for by in [0, bottom_top] {
            let p = placed_box(sat(xs[i], half(box_w[i])), by, box_w[i], BOX_H);
            draw_box(
                &mut canvas,
                &p,
                std::slice::from_ref(&labels[i]),
                Shape::Rect,
            );
        }
    }
    for (item, &row) in seq.items.iter().zip(&rows) {
        if let SeqItem::Note { anchor, text } = item {
            let (x, w) = note_geometry(&xs, *anchor, width_of(text));
            draw_box(
                &mut canvas,
                &placed_box(x, row, w, 3),
                std::slice::from_ref(text),
                Shape::Rect,
            );
        }
    }

    for &x in &xs {
        canvas.junction(x, BOX_H - 1, D);
        canvas.seg_v(x, BOX_H, bottom_top - 1);
        canvas.junction(x, bottom_top, U);
    }

    for (item, &row) in seq.items.iter().zip(&rows) {
        match item {
            SeqItem::Message {
                from,
                to,
                text,
                dashed,
                head,
            } => draw_message(
                &mut canvas,
                (*from, *to),
                text.as_deref(),
                *dashed,
                *head,
                &xs,
                row,
            ),
            SeqItem::Divider { text } => draw_divider(&mut canvas, text, row, canvas_w),
            SeqItem::Note { .. } => {}
        }
    }

    canvas.finalize_mask();
    Some(canvas)
}

fn row_height(item: &SeqItem) -> i64 {
    match item {
        SeqItem::Note { .. } => 4,
        SeqItem::Message { from, to, .. } if from == to => 4,
        SeqItem::Message { text: Some(_), .. } => 3,
        SeqItem::Divider { .. } | SeqItem::Message { text: None, .. } => 2,
    }
}

/// Geometry for a box drawn by position and size; ranks are irrelevant here.
fn placed_box(x: i64, y: i64, w: i64, h: i64) -> Placed {
    Placed {
        x,
        y,
        w,
        h,
        cx: x + half(w),
        cy: y + 1,
        rank: 0,
    }
}

fn draw_message(
    canvas: &mut Canvas,
    (from, to): (usize, usize),
    text: Option<&str>,
    dashed: bool,
    head: SeqHead,
    xs: &[i64],
    r: i64,
) {
    let line_ch = if dashed { '╌' } else { '─' };

    if from == to {
        // A stub that leaves the lifeline and returns two rows down.
        let x = xs[from];
        canvas.junction(x, r, R);
        canvas.set_char(x + 1, r, line_ch, Cls::Edge);
        canvas.set_char(x + 2, r, line_ch, Cls::Edge);
        canvas.set_char(x + 3, r, '╮', Cls::Edge);
        canvas.set_char(x + 3, r + 1, '│', Cls::Edge);
        let tip = match head {
            SeqHead::Cross => '×',
            SeqHead::Arrow => '◄',
        };
        canvas.set_char(x + 1, r + 2, tip, Cls::Edge);
        canvas.set_char(x + 2, r + 2, line_ch, Cls::Edge);
        canvas.set_char(x + 3, r + 2, '╯', Cls::Edge);
        if let Some(text) = text {
            draw_text_over_edges(canvas, text, x + 5, r + 1, Cls::Text);
        }
        return;
    }

    let x0 = xs[from];
    let x1 = xs[to];
    let rightward = x1 > x0;
    // A labelled message writes its text on `r` and draws the arrow below it.
    let arrow_row = if text.is_some() { r + 1 } else { r };
    let lo = x0.min(x1);
    let hi = x0.max(x1);

    canvas.junction(x0, arrow_row, if rightward { R } else { L });
    for x in lo + 1..hi {
        canvas.set_char(x, arrow_row, line_ch, Cls::Edge);
    }
    let head_ch = match (head, rightward) {
        (SeqHead::Cross, _) => '×',
        (SeqHead::Arrow, true) => '▶',
        (SeqHead::Arrow, false) => '◄',
    };
    let head_x = if rightward { x1 - 1 } else { x1 + 1 };
    canvas.set_char(head_x, arrow_row, head_ch, Cls::Edge);

    if let Some(text) = text {
        let span = hi - lo - 1;
        let t = fit_label(text, span.max(1) as usize);
        draw_text_over_edges(
            canvas,
            &t,
            lo + 1 + half(sat(span, width_of(&t))),
            r,
            Cls::Text,
        );
    }
}

/// A full-width rule labelling a `loop` / `alt` / `opt` block boundary.
fn draw_divider(canvas: &mut Canvas, text: &str, r: i64, canvas_w: i64) {
    for x in 0..canvas_w {
        canvas.set_char(x, r, '─', Cls::Edge);
    }
    let label = fit_label(text, sat(canvas_w, 4) as usize);
    draw_text_over_edges(canvas, &format!(" {label} "), 2, r, Cls::EdgeLabel);
}
