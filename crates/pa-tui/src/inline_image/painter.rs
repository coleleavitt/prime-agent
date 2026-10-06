//! The placement state machine: diffs each frame's plan against what is on
//! screen and writes only the changes.
//!
//! kitty keeps images in its own layer, keyed by id: an image is sent once
//! (`a=T`, or `a=t` when it first shows cropped), a move deletes its
//! placements and re-places the stored data (`a=p`), a band that left the
//! frame deletes its placements, a resize (ratatui's clear frees kitty's
//! copies) sends it again, and the surface's exit frees the data.
//! iTerm2 images live in the cells themselves: ratatui never repaints cells
//! it believes unchanged, so a moved or removed image's rows are repainted
//! from the frame before the new placement goes out, and an image shows
//! only whole (iTerm2 cannot crop; a band touching the last row would
//! scroll the screen).
//!
//! Inside tmux every escape rides the passthrough. kitty images there are
//! unicode placeholders the frame's cells draw (see `super::image_block_rows`),
//! so the painter only transmits each image once with a virtual placement
//! (`U=1`) sized to its block — moving, cropping, covering and pane switches
//! are tmux redrawing text — and re-creates the virtual placement when the
//! block's size changes. iTerm2 placements address the client's screen: the
//! cursor move rides inside the passthrough, offset by the pane's origin.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use super::payload::{KittyPayloadState, PayloadSource};
use super::plan::{markers, plan, Visible};
use crate::terminal_image::kitty_graphics::{
    placeholder_image_id, tmux_passthrough, tmux_write_with_payload, Command,
};
use crate::terminal_image::{
    allocate_image_id, delete_kitty_image, encode_iterm2, encode_kitty, kitty_delete_placements,
    kitty_place, kitty_transmit, ImageProtocol, ImageTerminal, ImageTransport, Iterm2Options,
    Iterm2Size, KittyCrop, KittyOptions,
};
use crate::Line;

#[derive(Debug, Clone, Copy)]
struct KittyImage {
    id: u32,
    /// Whether the terminal holds the data under `id`.
    sent: bool,
    /// The placeholder path's virtual placement: the block it was made for.
    virtual_block: Option<(u32, u32)>,
}

/// One placement on screen. iTerm2 placements remember the reserved rows
/// they were painted over: a changed row (a selection highlight) repainted
/// part of the image away.
#[derive(Debug, Clone, PartialEq)]
struct Placed {
    band: Visible,
    rows: Vec<Line>,
}

/// The placement state of one terminal screen.
#[derive(Debug, Default)]
pub(crate) struct Painter {
    size: (u16, u16),
    kitty: HashMap<u64, KittyImage>,
    placed: Vec<Placed>,
    /// The distinct previews placed since the last [`Painter::take_shown`].
    shown: HashSet<u64>,
}

/// CUP: rows and columns are 1-based.
fn move_to(out: &mut String, row: u16, column: u16) {
    let _ = write!(
        out,
        "\x1b[{};{}H",
        u32::from(row) + 1,
        u32::from(column) + 1
    );
}

impl Painter {
    /// Write the escapes that bring the screen in line with `frame`.
    pub(crate) fn paint(
        &mut self,
        terminal: ImageTerminal,
        frame: &[Line],
        size: (u16, u16),
        source: &dyn PayloadSource,
        out: &mut String,
    ) {
        if terminal.placeholders() {
            // tmux owns the cells (a resize redraws them); only the stored
            // images and their virtual placements are the painter's.
            self.size = size;
            self.paint_placeholders(frame, source, out);
            return;
        }
        let protocol = terminal.protocol;
        if size != self.size {
            // ratatui clears the screen on a resize (`ED 2`). iTerm2's images
            // go with the cells; kitty drops the placements and frees the
            // data of the images it cleared (kitty 0.46: a later `a=p` answers
            // ENOENT), so every image is sent again.
            let placed = std::mem::take(&mut self.placed);
            if protocol == ImageProtocol::Kitty {
                self.delete_kitty_placements(&placed, out);
                for image in self.kitty.values_mut() {
                    image.sent = false;
                }
            }
            self.size = size;
        }
        let wanted = plan(frame);
        match terminal.transport {
            ImageTransport::Direct if protocol == ImageProtocol::Kitty => {
                self.paint_kitty(&wanted, source, out);
            }
            ImageTransport::Direct => self.paint_iterm2(&wanted, frame, source, None, out),
            ImageTransport::Tmux {
                origin_row,
                origin_column,
            } => self.paint_iterm2(
                &wanted,
                frame,
                source,
                Some((origin_row, origin_column)),
                out,
            ),
        }
    }

    /// The placeholder path: transmit each image the frame's markers name
    /// once, with a virtual placement sized to its block (icat's `a=T,U=1`
    /// through the tmux passthrough); a resized block re-creates the virtual
    /// placement over the stored data. Images leaving the frame stay stored
    /// (their cells are tmux's text now); the surface's release frees them.
    fn paint_placeholders(&mut self, frame: &[Line], source: &dyn PayloadSource, out: &mut String) {
        for tag in markers(frame) {
            let KittyPayloadState::Ready(payload) = source.kitty(tag.key) else {
                continue;
            };
            let image = self.kitty.entry(tag.key).or_insert_with(|| KittyImage {
                id: placeholder_image_id(tag.key),
                sent: false,
                virtual_block: None,
            });
            let block = (tag.columns, tag.rows);
            if !image.sent {
                out.push_str(&tmux_write_with_payload(
                    &placeholder_transmit(image.id, block),
                    &payload.base64,
                ));
                image.sent = true;
            } else if image.virtual_block != Some(block) {
                let base = Command::new().key(b'q', 2).key(b'i', image.id);
                out.push_str(&tmux_write_with_payload(
                    &base.clone().key(b'a', 'd').key(b'd', 'i'),
                    "",
                ));
                out.push_str(&tmux_write_with_payload(
                    &base
                        .key(b'a', 'p')
                        .key(b'U', 1)
                        .key(b'c', block.0)
                        .key(b'r', block.1),
                    "",
                ));
            } else {
                continue;
            }
            image.virtual_block = Some(block);
            self.shown.insert(tag.key);
        }
    }

    fn delete_kitty_placements(&self, placed: &[Placed], out: &mut String) {
        for gone in placed {
            if let Some(image) = self.kitty.get(&gone.band.key) {
                out.push_str(&kitty_delete_placements(image.id));
            }
        }
    }

    fn paint_kitty(&mut self, wanted: &[Visible], source: &dyn PayloadSource, out: &mut String) {
        let (kept, gone): (Vec<Placed>, Vec<Placed>) = std::mem::take(&mut self.placed)
            .into_iter()
            .partition(|placed| wanted.contains(&placed.band));
        self.delete_kitty_placements(&gone, out);
        self.placed = kept;
        for band in wanted {
            if self.placed.iter().any(|placed| placed.band == *band) {
                continue;
            }
            let KittyPayloadState::Ready(payload) = source.kitty(band.key) else {
                continue;
            };
            let image = self.kitty.entry(band.key).or_insert_with(|| KittyImage {
                id: allocate_image_id(),
                sent: false,
                virtual_block: None,
            });
            move_to(out, band.row, band.column);
            if !image.sent && band.whole() {
                // TS `Image`'s placement: transmit and place in one command,
                // cursor left where it is.
                out.push_str(&encode_kitty(
                    &payload.base64,
                    &KittyOptions {
                        columns: Some(band.columns),
                        rows: Some(band.total),
                        image_id: Some(image.id),
                        move_cursor: false,
                    },
                ));
            } else {
                if !image.sent {
                    out.push_str(&kitty_transmit(&payload.base64, image.id));
                }
                let crop = (!band.whole()).then(|| {
                    let total = u64::from(band.total.max(1));
                    let height = u64::from(payload.height_px);
                    let y = u64::from(band.first) * height / total;
                    let end = u64::from(band.first + band.rows) * height / total;
                    KittyCrop {
                        y: y as u32,
                        width: payload.width_px,
                        height: (end - y).max(1) as u32,
                    }
                });
                out.push_str(&kitty_place(image.id, band.columns, band.rows, crop));
            }
            image.sent = true;
            self.shown.insert(band.key);
            self.placed.push(Placed {
                band: *band,
                rows: Vec::new(),
            });
        }
    }

    /// `tmux_origin`: the pane's origin on the client screen when the
    /// placement rides tmux's passthrough.
    fn paint_iterm2(
        &mut self,
        wanted: &[Visible],
        frame: &[Line],
        source: &dyn PayloadSource,
        tmux_origin: Option<(u16, u16)>,
        out: &mut String,
    ) {
        let height = self.size.1;
        let wanted: Vec<Placed> = wanted
            .iter()
            .filter(|band| band.whole() && u32::from(band.row) + band.total < u32::from(height))
            .map(|band| {
                let from = usize::from(band.row);
                Placed {
                    band: *band,
                    rows: frame[from..from + band.total as usize].to_vec(),
                }
            })
            .collect();
        let (kept, gone): (Vec<Placed>, Vec<Placed>) = std::mem::take(&mut self.placed)
            .into_iter()
            .partition(|placed| wanted.contains(placed));
        // Repaint every row a removed image covered from the frame: ratatui
        // believes those cells already hold it.
        let mut repainted: Vec<u16> = Vec::new();
        for gone in &gone {
            for row in gone.band.row..gone.band.row.saturating_add(gone.band.total as u16) {
                if repainted.contains(&row) {
                    continue;
                }
                repainted.push(row);
                move_to(out, row, 0);
                out.push_str("\x1b[2K");
                if let Some(line) = frame.get(usize::from(row)) {
                    let mut line = line.clone();
                    super::strip_markers(&mut line);
                    crate::osc133::strip(&mut line);
                    out.push_str(&crate::ansi::line_to_ansi(&line));
                }
            }
        }
        let overlaps = |placed: &Placed| {
            let rows = placed.band.row..placed.band.row.saturating_add(placed.band.total as u16);
            repainted.iter().any(|row| rows.contains(row))
        };
        self.placed = kept
            .into_iter()
            .filter(|placed| !overlaps(placed))
            .collect();
        for placement in wanted {
            if self.placed.contains(&placement) {
                continue;
            }
            let Some(data) = source.file(placement.band.key) else {
                continue;
            };
            // TS `renderImage`'s iTerm2 form: the width in cells, the height
            // from the aspect ratio.
            let image = encode_iterm2(
                &data,
                &Iterm2Options {
                    width: Some(Iterm2Size::Cells(placement.band.columns)),
                    height: Some(Iterm2Size::Auto),
                    ..Iterm2Options::default()
                },
            );
            if let Some((origin_row, origin_column)) = tmux_origin {
                // tmux writes a passthrough at the client's cursor, which it
                // never moved for the pane: the move rides inside.
                let mut placed = String::new();
                move_to(
                    &mut placed,
                    placement.band.row.saturating_add(origin_row),
                    placement.band.column.saturating_add(origin_column),
                );
                placed.push_str(&image);
                out.push_str(&tmux_passthrough(&placed));
            } else {
                move_to(out, placement.band.row, placement.band.column);
                out.push_str(&image);
            }
            self.shown.insert(placement.band.key);
            self.placed.push(placement);
        }
    }

    /// The count of distinct previews placed since the last take.
    pub(crate) fn take_shown(&mut self) -> u64 {
        let count = self.shown.len() as u64;
        self.shown.clear();
        count
    }

    /// Take every image off the screen and free kitty's copies (the surface
    /// leaves or hands the alternate screen on); the next frame starts over.
    pub(crate) fn release(&mut self, terminal: ImageTerminal, out: &mut String) {
        for image in self.kitty.values().filter(|image| image.sent) {
            if terminal.tmux() {
                let delete = Command::new()
                    .key(b'a', 'd')
                    .key(b'q', 2)
                    .key(b'd', 'I')
                    .key(b'i', image.id);
                out.push_str(&tmux_write_with_payload(&delete, ""));
            } else {
                out.push_str(&delete_kitty_image(image.id));
            }
        }
        self.kitty.clear();
        self.placed.clear();
        self.size = (0, 0);
    }
}

/// icat's frame-0 transmit for a placeholder image: transmit and create the
/// virtual placement in one command (`a=T,U=1`), quiet, the block's size.
fn placeholder_transmit(id: u32, (columns, rows): (u32, u32)) -> Command {
    Command::new()
        .key(b'a', 'T')
        .key(b'q', 2)
        .key(b'f', 100)
        .key(b'U', 1)
        .key(b'c', columns)
        .key(b'r', rows)
        .key(b'i', id)
}
