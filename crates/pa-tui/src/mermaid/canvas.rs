//! The cell grid diagrams are drawn on. Ported from grok-mermaid 0.2.3 `canvas.ts`
//! (Apache-2.0; see `LICENSE-grok-mermaid`).
//!
//! Coordinates are `i64` like the package's JS numbers: the index rules (a column past the
//! width is ignored, a negative index writes nowhere) reproduce the JS array semantics.

use super::width::measured;
use super::{ArtSpan, Cls};

/// What one cell holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Glyph {
    Char(char),
    /// A multi-code-point grapheme cluster.
    Cluster(Box<str>),
    /// The trailing column of a wide glyph: claims layout, emits nothing.
    Cont,
}

pub(super) const BLANK: Glyph = Glyph::Char(' ');

impl Glyph {
    fn of(cluster: &str) -> Self {
        let mut chars = cluster.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Glyph::Char(c),
            _ => Glyph::Cluster(cluster.into()),
        }
    }

    pub(super) fn is_blank(&self) -> bool {
        *self == BLANK
    }
}

/// Connection direction bits, combined into a box-drawing glyph by [`mask_char`].
pub(super) const U: u8 = 1;
pub(super) const D: u8 = 2;
pub(super) const L: u8 = 4;
pub(super) const R: u8 = 8;

/// Line styles, tracked per cell so crossing edges keep their own stroke.
pub(super) const STY_DOT: u8 = 1;
pub(super) const STY_THICK: u8 = 2;
pub(super) const STY_SOLID: u8 = 4;

/// A grid of cells. Edges accumulate direction bits rather than glyphs so crossings and
/// junctions resolve whatever order they are drawn in; [`Canvas::finalize_mask`] turns the
/// bits into characters. `occupied` marks box cells edge bits must not overwrite.
#[derive(Debug, Clone)]
pub(super) struct Canvas {
    pub(super) w: i64,
    pub(super) h: i64,
    pub(super) ch: Vec<Glyph>,
    pub(super) cls: Vec<Cls>,
    pub(super) mask: Vec<u8>,
    style: Vec<u8>,
    pub(super) occupied: Vec<u8>,
    pub(super) cur_style: u8,
}

impl Canvas {
    pub(super) fn new(w: i64, h: i64) -> Self {
        let n = (w * h).max(0) as usize;
        Self {
            w,
            h,
            ch: vec![BLANK; n],
            cls: vec![Cls::None; n],
            mask: vec![0; n],
            style: vec![0; n],
            occupied: vec![0; n],
            cur_style: STY_SOLID,
        }
    }

    /// The cell index of `(x, y)` when it names a cell of the grid's backing store.
    pub(super) fn idx(&self, x: i64, y: i64) -> Option<usize> {
        let i = y * self.w + x;
        (0..self.w * self.h).contains(&i).then_some(i as usize)
    }

    /// The writable index the JS `set` guard admits: columns and rows past the far edge
    /// are skipped; anything else lands wherever `y * w + x` points.
    fn write_idx(&self, x: i64, y: i64) -> Option<usize> {
        if x >= self.w || y >= self.h {
            return None;
        }
        self.idx(x, y)
    }

    pub(super) fn set(&mut self, x: i64, y: i64, glyph: Glyph, cls: Cls) {
        if let Some(i) = self.write_idx(x, y) {
            self.ch[i] = glyph;
            self.cls[i] = cls;
        }
    }

    pub(super) fn set_char(&mut self, x: i64, y: i64, c: char, cls: Cls) {
        self.set(x, y, Glyph::Char(c), cls);
    }

    /// Accumulate direction bits on a free cell; `border` cells keep their class.
    pub(super) fn add_bits(&mut self, x: i64, y: i64, bits: u8, cls: Cls) {
        let Some(i) = self.write_idx(x, y) else {
            return;
        };
        if self.occupied[i] != 0 {
            return;
        }
        self.mask[i] |= bits;
        self.style[i] |= self.cur_style;
        if self.cls[i] != Cls::Border {
            self.cls[i] = cls;
        }
    }

    /// Mark a cell as part of a box.
    pub(super) fn occupy(&mut self, x: i64, y: i64) {
        if let Some(i) = self.idx(x, y) {
            self.occupied[i] = 1;
        }
    }

    /// Stamp a finished sub-canvas (a subgraph frame's contents) at an offset.
    pub(super) fn blit(&mut self, sub: &Canvas, ox: i64, oy: i64) {
        for sy in 0..sub.h {
            for sx in 0..sub.w {
                let Some(di) = self.write_idx(ox + sx, oy + sy) else {
                    continue;
                };
                let si = (sy * sub.w + sx) as usize;
                self.ch[di] = sub.ch[si].clone();
                self.cls[di] = sub.cls[si];
                self.style[di] = sub.style[si];
                self.occupied[di] = 1;
            }
        }
    }

    /// Add direction bits even to an occupied cell, so an edge can meet a border.
    pub(super) fn junction(&mut self, x: i64, y: i64, bits: u8) {
        let Some(i) = self.write_idx(x, y) else {
            return;
        };
        self.mask[i] |= bits;
        if self.cls[i] != Cls::Border {
            self.cls[i] = Cls::Edge;
        }
    }

    pub(super) fn seg_v(&mut self, x: i64, y0: i64, y1: i64) {
        let (a, b) = (y0.min(y1), y0.max(y1));
        for y in a..=b {
            let mut bits = 0;
            if y > a {
                bits |= U;
            }
            if y < b {
                bits |= D;
            }
            self.add_bits(x, y, bits, Cls::Edge);
        }
    }

    pub(super) fn seg_h(&mut self, y: i64, x0: i64, x1: i64) {
        let (a, b) = (x0.min(x1), x0.max(x1));
        for x in a..=b {
            let mut bits = 0;
            if x > a {
                bits |= L;
            }
            if x < b {
                bits |= R;
            }
            self.add_bits(x, y, bits, Cls::Edge);
        }
    }

    /// Resolve accumulated direction bits into glyphs, honouring line style.
    pub(super) fn finalize_mask(&mut self) {
        for i in 0..self.ch.len() {
            if self.mask[i] != 0 && self.ch[i].is_blank() {
                let c = mask_char(self.mask[i]);
                let c = match self.style[i] {
                    STY_DOT => dotted_char(c),
                    STY_THICK => thick_char(c),
                    _ => c,
                };
                self.ch[i] = Glyph::Char(c);
            }
        }
    }

    /// Mirror top-to-bottom for `BT`; text stays readable, box glyphs flip.
    pub(super) fn flip_vertical(&mut self) {
        let w = self.w as usize;
        let h = self.h as usize;
        for y in 0..h / 2 {
            let y2 = h - 1 - y;
            for x in 0..w {
                self.ch.swap(y * w + x, y2 * w + x);
                self.cls.swap(y * w + x, y2 * w + x);
            }
        }
        for glyph in &mut self.ch {
            if let Glyph::Char(c) = glyph {
                *c = flip_glyph_v(*c);
            }
        }
    }

    /// Mirror left-to-right for `RL`, then reverse each text/label run back to reading
    /// order.
    pub(super) fn flip_horizontal(&mut self) {
        let w = self.w as usize;
        let h = self.h as usize;
        for y in 0..h {
            for x in 0..w / 2 {
                let x2 = w - 1 - x;
                self.ch.swap(y * w + x, y * w + x2);
                self.cls.swap(y * w + x, y * w + x2);
            }
        }
        for glyph in &mut self.ch {
            if let Glyph::Char(c) = glyph {
                *c = flip_glyph_h(*c);
            }
        }
        for y in 0..h {
            let mut x = 0;
            while x < w {
                let cls = self.cls[y * w + x];
                if matches!(cls, Cls::Text | Cls::EdgeLabel) {
                    let start = y * w + x;
                    while x < w && self.cls[y * w + x] == cls {
                        x += 1;
                    }
                    self.ch[start..y * w + x].reverse();
                } else {
                    x += 1;
                }
            }
        }
    }

    /// Group each row into runs of one class, dropping wide-glyph continuations, with the
    /// leading and trailing empty rows removed. Returns the rows and the widest row's width.
    pub(super) fn to_lines(&self) -> (Vec<Vec<ArtSpan>>, usize) {
        let w = self.w as usize;
        let mut rows: Vec<Vec<ArtSpan>> = Vec::with_capacity(self.h as usize);
        let mut width = 0;
        for y in 0..self.h as usize {
            let row = &self.ch[y * w..(y + 1) * w];
            // A trailing continuation counts as painted: the row reaches that column.
            let last = row.iter().rposition(|g| !g.is_blank()).map_or(0, |x| x + 1);
            width = width.max(last);
            let mut spans: Vec<ArtSpan> = Vec::new();
            let mut run = String::new();
            let mut run_cls = Cls::None;
            for (x, glyph) in row.iter().enumerate().take(last) {
                let text = match glyph {
                    Glyph::Cont => continue,
                    Glyph::Char(c) => CharOrStr::Char(*c),
                    Glyph::Cluster(s) => CharOrStr::Str(s),
                };
                let cls = self.cls[y * w + x];
                if cls != run_cls && !run.is_empty() {
                    spans.push(ArtSpan {
                        text: std::mem::take(&mut run),
                        cls: run_cls,
                    });
                }
                run_cls = cls;
                match text {
                    CharOrStr::Char(c) => run.push(c),
                    CharOrStr::Str(s) => run.push_str(s),
                }
            }
            if !run.is_empty() {
                spans.push(ArtSpan {
                    text: run,
                    cls: run_cls,
                });
            }
            rows.push(spans);
        }
        let first = rows
            .iter()
            .position(|r| !r.is_empty())
            .unwrap_or(rows.len());
        let end = rows
            .iter()
            .rposition(|r| !r.is_empty())
            .map_or(first, |i| i + 1);
        rows.truncate(end);
        rows.drain(..first);
        (rows, width)
    }
}

enum CharOrStr<'a> {
    Char(char),
    Str(&'a str),
}

/// Paint `text` at `(x, y)`, one grapheme cluster per cell; a wide cluster claims its
/// continuation cells, a zero-width one paints nothing.
pub(super) fn draw_text(canvas: &mut Canvas, text: &str, x: i64, y: i64, cls: Cls) {
    let mut cur = x;
    for (cluster, cw) in measured(text) {
        if cw == 0 {
            continue;
        }
        canvas.set(cur, y, Glyph::of(cluster), cls);
        for k in 1..cw as i64 {
            canvas.set(cur + k, y, Glyph::Cont, cls);
        }
        cur += cw as i64;
    }
}

/// Paint `text` at `(x, y)`, clearing any edge bits underneath first so the text wins
/// over a drawn line.
pub(super) fn draw_text_over_edges(canvas: &mut Canvas, text: &str, x: i64, y: i64, cls: Cls) {
    let mut cur = x;
    for (cluster, cw) in measured(text) {
        if cw == 0 {
            continue;
        }
        for k in 0..cw as i64 {
            if cur + k < canvas.w && y < canvas.h {
                if let Some(i) = canvas.idx(cur + k, y) {
                    canvas.mask[i] = 0;
                }
            }
            let glyph = if k == 0 {
                Glyph::of(cluster)
            } else {
                Glyph::Cont
            };
            canvas.set(cur + k, y, glyph, cls);
        }
        cur += cw as i64;
    }
}

pub(super) fn mask_char(mask: u8) -> char {
    const UD: u8 = U | D;
    const LR: u8 = L | R;
    const DR: u8 = D | R;
    const DL: u8 = D | L;
    const UR: u8 = U | R;
    const UL: u8 = U | L;
    const UDR: u8 = U | D | R;
    const UDL: u8 = U | D | L;
    const DLR: u8 = D | L | R;
    const ULR: u8 = U | L | R;
    match mask {
        0 => ' ',
        U | D | UD => '│',
        L | R | LR => '─',
        DR => '┌',
        DL => '┐',
        UR => '└',
        UL => '┘',
        UDR => '├',
        UDL => '┤',
        DLR => '┬',
        ULR => '┴',
        _ => '┼',
    }
}

fn dotted_char(c: char) -> char {
    match c {
        '─' => '╌',
        '│' => '╎',
        other => other,
    }
}

fn thick_char(c: char) -> char {
    match c {
        '─' => '━',
        '│' => '┃',
        '┌' => '┏',
        '┐' => '┓',
        '└' => '┗',
        '┘' => '┛',
        '├' => '┣',
        '┤' => '┫',
        '┬' => '┳',
        '┴' => '┻',
        '┼' => '╋',
        other => other,
    }
}

fn flip_glyph_v(c: char) -> char {
    match c {
        '┌' => '└',
        '└' => '┌',
        '┐' => '┘',
        '┘' => '┐',
        '┏' => '┗',
        '┗' => '┏',
        '┓' => '┛',
        '┛' => '┓',
        '╭' => '╰',
        '╰' => '╭',
        '╮' => '╯',
        '╯' => '╮',
        '┬' => '┴',
        '┴' => '┬',
        '┳' => '┻',
        '┻' => '┳',
        '▼' => '▲',
        '▲' => '▼',
        '▽' => '△',
        '△' => '▽',
        other => other,
    }
}

fn flip_glyph_h(c: char) -> char {
    match c {
        '┌' => '┐',
        '┐' => '┌',
        '└' => '┘',
        '┘' => '└',
        '┏' => '┓',
        '┓' => '┏',
        '┗' => '┛',
        '┛' => '┗',
        '╭' => '╮',
        '╮' => '╭',
        '╰' => '╯',
        '╯' => '╰',
        '├' => '┤',
        '┤' => '├',
        '┣' => '┫',
        '┫' => '┣',
        '▶' => '◄',
        '◄' => '▶',
        '▷' => '◁',
        '◁' => '▷',
        other => other,
    }
}
