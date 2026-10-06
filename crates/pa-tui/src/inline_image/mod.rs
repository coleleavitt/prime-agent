//! Inline images in the transcript: the presented-artifact preview (the one
//! row that shows a real picture; every tool-result image stays a textual
//! fallback, as in TS) on terminals that speak the kitty graphics protocol
//! or iTerm2's inline images (`crate::terminal_image`).
//!
//! The image never rides ratatui's cells. The row renders a block of blank
//! reserved rows sized from the cell size, each tagged with a zero-width
//! marker (an APC string terminals ignore, stripped before the cell paint).
//! After the frame flush, [`paint_frame`] scans the composed frame for the
//! markers and the [`Painter`] places the image over the reserved cells
//! with cursor positioning, re-placing it on scroll and resize and deleting
//! it when its rows leave the frame. Inside tmux (passthrough on, a kitty
//! client) the reserved cells instead hold kitty's unicode placeholders,
//! drawn by ratatui like any text: tmux owns those cells, so the image
//! scrolls, clips, and survives pane switches with them, and the painter
//! only transmits each image once through the passthrough. Without a
//! protocol (unknown terminals, screen, tmux with passthrough off), and in
//! the exit flush's scrollback, the row keeps its textual fallback.

mod painter;
mod payload;
mod plan;

pub(crate) use painter::Painter;
pub(crate) use payload::{payload_ready, GlobalSource};

use std::cell::Cell;
use std::sync::{Arc, LazyLock, Mutex};

use crate::terminal_image::kitty_graphics::{
    is_placeholder_cell, placeholder_cell, placeholder_image_id, placeholder_rgb,
    MAX_PLACEHOLDER_CELLS, PLACEHOLDER,
};
use crate::terminal_image::{
    calculate_image_rows, cell_dimensions, image_protocol, image_terminal, CellDimensions,
    ImageDimensions, ImageProtocol, ImageTerminal,
};
use crate::{Line, Span};

/// The column the image starts at: under the branch indent of the panel body.
pub(crate) const IMAGE_COLUMN: usize = crate::branch::BRANCH_INDENT.len();
/// The widest preview in cells (TS `Image`'s default `maxWidthCells`).
const MAX_IMAGE_COLUMNS: u32 = 60;
/// The tallest preview in rows: a portrait preview narrows to fit instead
/// of filling several screens.
const MAX_IMAGE_ROWS: u32 = 24;
const _: () = assert!(MAX_IMAGE_ROWS < MAX_PLACEHOLDER_CELLS);
const _: () = assert!(MAX_IMAGE_COLUMNS < MAX_PLACEHOLDER_CELLS);
/// Narrower than this the preview stays the textual fallback.
const MIN_IMAGE_COLUMNS: u32 = 8;

/// A preview's base64 file bytes, shared by the row and the payload cache.
#[derive(Clone, PartialEq, Eq)]
pub struct ImageData(Arc<str>);

impl std::fmt::Debug for ImageData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ImageData({} base64 chars)", self.0.len())
    }
}

/// An image a transcript row may place: the base64 file bytes (PNG, JPEG,
/// GIF, or WebP), their type, and their pixel size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelImage {
    key: u64,
    data: ImageData,
    mime_type: String,
    dimensions: ImageDimensions,
}

impl PanelImage {
    /// Wrap a decoded preview. The content key is computed here, once, when
    /// the row decodes — never on the paint path.
    #[must_use]
    pub fn new(data: &str, mime_type: &str, dimensions: ImageDimensions) -> Self {
        use std::hash::{Hash, Hasher};
        let data: Arc<str> = Arc::from(data);
        let mut hasher = std::hash::DefaultHasher::new();
        mime_type.hash(&mut hasher);
        data.hash(&mut hasher);
        let key = hasher.finish();
        payload::register(key, &data, mime_type);
        Self {
            key,
            data: ImageData(data),
            mime_type: mime_type.to_string(),
            dimensions,
        }
    }

    #[must_use]
    pub fn dimensions(&self) -> ImageDimensions {
        self.dimensions
    }
}

/// The reserved cell area of a placed preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImageBlock {
    pub(crate) columns: u32,
    pub(crate) rows: u32,
}

/// Whether `protocol` can show `mime_type`: every preview type the kernel
/// writes (PNG, JPEG, GIF, WebP) on both — iTerm2 takes PNG/JPEG/GIF as
/// they are and WebP as a PNG transcode; kitty takes PNG as it is, JPEG as
/// a PNG transcode, and GIF/WebP as their first frame's raw RGBA (see
/// `payload`). A failed transcode falls back per image.
fn protocol_accepts(_protocol: ImageProtocol, mime_type: &str) -> bool {
    matches!(
        mime_type,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// The preview's reserved block at `width`, or `None` when the row keeps
/// its textual fallback (no protocol, an unsupported type, a failed
/// transcode, the exit flush's scrollback, or too narrow a pane).
pub(crate) fn image_block(image: &PanelImage, width: usize) -> Option<ImageBlock> {
    if text_fallback_active() {
        return None;
    }
    let protocol = image_protocol()?;
    if !protocol_accepts(protocol, &image.mime_type) || payload::failed(image.key) {
        return None;
    }
    block_geometry(image.dimensions, width, cell_dimensions())
}

/// The block an image of `dims` covers under a `width`-column panel: TS
/// `Image`'s `min(width, 60)` columns and `calculateImageRows` rows, capped
/// at [`MAX_IMAGE_ROWS`] by narrowing the columns (the aspect ratio holds).
fn block_geometry(dims: ImageDimensions, width: usize, cell: CellDimensions) -> Option<ImageBlock> {
    if dims.width_px == 0 || dims.height_px == 0 {
        return None;
    }
    let available = u32::try_from(width.saturating_sub(IMAGE_COLUMN)).unwrap_or(u32::MAX);
    let mut columns = available.min(MAX_IMAGE_COLUMNS);
    if columns < MIN_IMAGE_COLUMNS {
        return None;
    }
    let mut rows = calculate_image_rows(dims, columns, cell);
    if rows > MAX_IMAGE_ROWS {
        let fit = f64::from(MAX_IMAGE_ROWS) * f64::from(cell.height_px) * f64::from(dims.width_px)
            / (f64::from(dims.height_px) * f64::from(cell.width_px.max(1)));
        columns = (fit.floor() as u32).clamp(1, columns);
        rows = calculate_image_rows(dims, columns, cell).min(MAX_IMAGE_ROWS);
    }
    Some(ImageBlock { columns, rows })
}

/// The reserved rows of a placed preview: blank rows at the branch indent,
/// each carrying its zero-width placement marker.
///
/// Under tmux the reserved cells hold kitty's unicode placeholders instead
/// of blanks (icat's `write_unicode_placeholder`): each cell numbers its row
/// and column with diacritics and carries the image id in its foreground
/// colour, so the cells themselves are the image wherever tmux draws them.
pub(crate) fn image_block_rows(image: &PanelImage, block: ImageBlock) -> Vec<Line> {
    let placeholders = image_terminal().is_some_and(ImageTerminal::placeholders);
    let id = placeholder_image_id(image.key);
    let (r, g, b) = placeholder_rgb(id);
    let style = ratatui::style::Style::default().fg(ratatui::style::Color::Rgb(r, g, b));
    (0..block.rows)
        .map(|index| {
            let mut row = vec![
                Span::raw(marker(&Marker {
                    key: image.key,
                    index,
                    rows: block.rows,
                    column: IMAGE_COLUMN as u32,
                    columns: block.columns,
                })),
                Span::raw(crate::branch::BRANCH_INDENT),
            ];
            if placeholders {
                row.push(Span {
                    style,
                    content: (0..block.columns)
                        .map(|column| placeholder_cell(id, index, column))
                        .collect(),
                });
            }
            row
        })
        .collect()
}

/// Blank the placeholder cells of a row for the plain-text dumps (the
/// selection copy, the headless frame text): one space per cell.
pub(crate) fn blank_placeholders(line: &mut Line) {
    use unicode_segmentation::UnicodeSegmentation;
    for span in line.iter_mut() {
        if !span.content.contains(PLACEHOLDER) {
            continue;
        }
        span.content = span
            .content
            .graphemes(true)
            .map(|cell| if is_placeholder_cell(cell) { " " } else { cell })
            .collect();
    }
}

const MARKER_PREFIX: &str = "\x1b_pa-image;";
const MARKER_SUFFIX: &str = "\x1b\\";

/// One reserved row's placement tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Marker {
    pub(crate) key: u64,
    /// This row's index inside the block.
    pub(crate) index: u32,
    pub(crate) rows: u32,
    pub(crate) column: u32,
    pub(crate) columns: u32,
}

fn marker(tag: &Marker) -> String {
    format!(
        "{MARKER_PREFIX}{:016x};{};{};{};{}{MARKER_SUFFIX}",
        tag.key, tag.index, tag.rows, tag.column, tag.columns
    )
}

/// Parse a span that is exactly one placement marker.
pub(crate) fn parse_marker(content: &str) -> Option<Marker> {
    let body = content
        .strip_prefix(MARKER_PREFIX)?
        .strip_suffix(MARKER_SUFFIX)?;
    let mut fields = body.split(';');
    let key = u64::from_str_radix(fields.next()?, 16).ok()?;
    let mut number = || fields.next()?.parse::<u32>().ok();
    let tag = Marker {
        key,
        index: number()?,
        rows: number()?,
        column: number()?,
        columns: number()?,
    };
    (fields.next().is_none() && tag.index < tag.rows).then_some(tag)
}

/// Remove placement markers from a row (the cell paint and the plain-text
/// dumps never see them).
pub(crate) fn strip_markers(line: &mut Line) {
    line.retain(|span| !span.content.starts_with(MARKER_PREFIX));
}

thread_local! {
    /// The exit flush's scope: rows render their textual fallback.
    static TEXT_FALLBACK: Cell<bool> = const { Cell::new(false) };
}

/// Render `f` with every preview as its textual fallback: the exit flush
/// writes into native scrollback, where a placed image would not survive.
pub(crate) fn with_text_fallback<T>(f: impl FnOnce() -> T) -> T {
    TEXT_FALLBACK.with(|flag| {
        let previous = flag.replace(true);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        flag.set(previous);
        result.unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    })
}

fn text_fallback_active() -> bool {
    TEXT_FALLBACK.with(Cell::get)
}

/// Everything a preview's row geometry depends on besides the width: the
/// transcript's layout caches key on it (TS keeps the cell-size version in
/// `Image`'s cache key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayoutKey {
    protocol: Option<ImageProtocol>,
    placeholders: bool,
    cell: CellDimensions,
    failures: u64,
    text_fallback: bool,
}

pub(crate) fn layout_key() -> LayoutKey {
    LayoutKey {
        protocol: image_protocol(),
        placeholders: image_terminal().is_some_and(ImageTerminal::placeholders),
        cell: cell_dimensions(),
        failures: payload::failure_epoch(),
        text_fallback: text_fallback_active(),
    }
}

/// The process's placement state (one terminal).
static PAINTER: LazyLock<Mutex<Painter>> = LazyLock::new(|| Mutex::new(Painter::default()));

fn painter() -> std::sync::MutexGuard<'static, Painter> {
    PAINTER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The image escapes for one painted frame (empty without a protocol or
/// when nothing changed). Runs after the cell flush; reads no disk.
pub(crate) fn paint_frame(frame: &[Line], width: u16, height: u16) -> String {
    let Some(terminal) = image_terminal() else {
        return String::new();
    };
    let mut out = String::new();
    painter().paint(terminal, frame, (width, height), &GlobalSource, &mut out);
    out
}

/// The image terminal changed (a probe answered): wake the session loop so
/// the next frame lays the previews out for it.
pub(crate) fn notify_terminal_changed() {
    payload::notify_ready();
}

/// Take every placed image off the screen and free kitty's copies: the
/// surface is leaving the alternate screen or handing it to another view.
/// Never blocks: the exit restore also runs from panic and signal paths,
/// where a paint may still hold the state.
pub(crate) fn release_screen(out: &mut impl std::io::Write) {
    let Some(terminal) = image_terminal() else {
        return;
    };
    let mut painter = match PAINTER.try_lock() {
        Ok(painter) => painter,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    let mut escapes = String::new();
    painter.release(terminal, &mut escapes);
    if !escapes.is_empty() {
        let _ = out.write_all(escapes.as_bytes());
        let _ = out.flush();
    }
}

/// The run's inline-image adoption for `tui exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShownImages {
    /// The terminal's protocol: `kitty`, `iterm2`, or `off`.
    pub protocol: &'static str,
    /// The distinct previews placed since the last take.
    pub count: u64,
}

/// Take the inline-image adoption counters (each preview counts once per
/// run, never per repaint).
#[must_use]
pub fn take_shown() -> ShownImages {
    ShownImages {
        protocol: match image_protocol() {
            Some(ImageProtocol::Kitty) => "kitty",
            Some(ImageProtocol::Iterm2) => "iterm2",
            None => "off",
        },
        count: painter().take_shown(),
    }
}

#[cfg(test)]
mod tests;
