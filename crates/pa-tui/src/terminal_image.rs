//! Terminal images (TS `terminal-image.ts`): capability detection, the
//! cell-size state, the kitty graphics (APC `ESC _G`) and iTerm2 (OSC 1337)
//! encoders, pixel-dimension parsing, and the textual fallback row.
//! Tool-result image rows stay the textual fallback (TS mounts their
//! `Image` components with `fallbackOnly`), so the TUI never decodes a
//! whole tool payload: [`get_image_dimensions_prefix`] reads dimensions
//! from a bounded prefix. The presented-artifact preview is the one row that
//! places a real image (see `crate::inline_image`). [`is_image_line`] stays
//! for the exit-flush scrollback.

use std::cell::Cell;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// The inline image protocol a terminal speaks (TS `ImageProtocol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageProtocol {
    /// The kitty graphics protocol (kitty, Ghostty, `WezTerm`).
    Kitty,
    /// iTerm2's inline images (`OSC 1337 ; File=`).
    Iterm2,
}

/// The image protocol `env` describes (TS `detectCapabilities().images`):
/// tmux and screen always fall back (TS leaves `images: null` there — the
/// passthrough is opt-in and wraps sequences differently); kitty, Ghostty
/// and `WezTerm` speak kitty graphics; iTerm2 its OSC 1337; anything else
/// falls back. An empty variable reads as unset (the TS truthiness test).
pub fn detect_image_protocol(env: impl Fn(&str) -> Option<String>) -> Option<ImageProtocol> {
    let var = |name: &str| env(name).filter(|value| !value.is_empty());
    let term_program = var("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    let term = var("TERM").unwrap_or_default().to_lowercase();
    if var("TMUX").is_some() || term.starts_with("tmux") || term.starts_with("screen") {
        return None;
    }
    if var("KITTY_WINDOW_ID").is_some() || term_program == "kitty" {
        return Some(ImageProtocol::Kitty);
    }
    if term_program == "ghostty"
        || term.contains("ghostty")
        || var("GHOSTTY_RESOURCES_DIR").is_some()
    {
        return Some(ImageProtocol::Kitty);
    }
    if var("WEZTERM_PANE").is_some() || term_program == "wezterm" {
        return Some(ImageProtocol::Kitty);
    }
    if var("ITERM_SESSION_ID").is_some() || term_program == "iterm.app" {
        return Some(ImageProtocol::Iterm2);
    }
    None
}

thread_local! {
    /// A test's image-protocol override.
    static PROTOCOL_OVERRIDE: Cell<Option<ForcedProtocol>> = const { Cell::new(None) };
    /// A test's cell-size override.
    static CELL_OVERRIDE: Cell<Option<CellDimensions>> = const { Cell::new(None) };
}

/// The process's image protocol (TS `getCapabilities().images`), detected
/// once from the environment.
pub fn image_protocol() -> Option<ImageProtocol> {
    static DETECTED: OnceLock<Option<ImageProtocol>> = OnceLock::new();
    if let Some(ForcedProtocol(forced)) = PROTOCOL_OVERRIDE.with(Cell::get) {
        return forced;
    }
    *DETECTED.get_or_init(|| detect_image_protocol(|name| std::env::var(name).ok()))
}

/// A test-forced protocol (`None`: the fallback terminal).
#[derive(Debug, Clone, Copy)]
struct ForcedProtocol(Option<ImageProtocol>);

/// Force this thread's image protocol (`None`: a terminal without one).
#[cfg(test)]
pub(crate) fn set_image_protocol_override(protocol: Option<ImageProtocol>) {
    PROTOCOL_OVERRIDE.with(|cell| cell.set(Some(ForcedProtocol(protocol))));
}

/// Restore this thread's environment detection.
#[cfg(test)]
pub(crate) fn clear_image_protocol_override() {
    PROTOCOL_OVERRIDE.with(|cell| cell.set(None));
}

/// One terminal cell's pixel size (TS `CellDimensions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

/// TS's default cell size until the terminal reports its own.
pub const DEFAULT_CELL_DIMENSIONS: CellDimensions = CellDimensions {
    width_px: 9,
    height_px: 18,
};

/// The packed cell size (`width << 32 | height`; zero = the default).
static CELL_DIMENSIONS: AtomicU64 = AtomicU64::new(0);

/// The current cell size (TS `getCellDimensions`).
pub fn cell_dimensions() -> CellDimensions {
    if let Some(overridden) = CELL_OVERRIDE.with(Cell::get) {
        return overridden;
    }
    match CELL_DIMENSIONS.load(Ordering::Relaxed) {
        0 => DEFAULT_CELL_DIMENSIONS,
        packed => CellDimensions {
            width_px: (packed >> 32) as u32,
            height_px: packed as u32,
        },
    }
}

/// Record the terminal's cell size (TS `setCellDimensions`; image rows keep
/// the size in their layout key). A zero dimension is ignored.
pub fn set_cell_dimensions(dims: CellDimensions) {
    if dims.width_px == 0 || dims.height_px == 0 {
        return;
    }
    let packed = u64::from(dims.width_px) << 32 | u64::from(dims.height_px);
    CELL_DIMENSIONS.store(packed, Ordering::Relaxed);
}

/// Override this thread's cell size (`None` restores the process value).
#[cfg(test)]
pub(crate) fn set_cell_dimensions_override(dims: Option<CellDimensions>) {
    CELL_OVERRIDE.with(|cell| cell.set(dims));
}

/// Read the cell size from the tty's pixel geometry. TS asks the terminal
/// (`CSI 16 t`) and parses the in-band reply; the port reads the same
/// numbers from `TIOCGWINSZ`, which kitty, Ghostty, `WezTerm` and iTerm2 fill
/// in, so no reply has to be fished out of the key stream. Like TS it runs
/// only when the terminal places images; a tty reporting no pixels keeps
/// the previous (default) size.
pub fn refresh_cell_dimensions() {
    if image_protocol().is_none() {
        return;
    }
    if let Ok(size) = crossterm::terminal::window_size() {
        if size.columns > 0 && size.rows > 0 {
            set_cell_dimensions(CellDimensions {
                width_px: u32::from(size.width) / u32::from(size.columns),
                height_px: u32::from(size.height) / u32::from(size.rows),
            });
        }
    }
}

/// The kitty graphics payload chunk size (TS `CHUNK_SIZE`).
const KITTY_CHUNK_SIZE: usize = 4096;

/// Options for [`encode_kitty`] (TS `encodeKitty` options).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyOptions {
    pub columns: Option<u32>,
    pub rows: Option<u32>,
    pub image_id: Option<u32>,
    /// Whether kitty applies its default cursor movement after placement.
    pub move_cursor: bool,
}

impl Default for KittyOptions {
    fn default() -> Self {
        Self {
            columns: None,
            rows: None,
            image_id: None,
            move_cursor: true,
        }
    }
}

/// One kitty command over a base64 payload, chunked at 4096 characters
/// exactly like TS `encodeKitty`: the first chunk carries `params,m=1`, the
/// middle ones `m=1`, the last `m=0`.
fn kitty_chunked(params: &str, base64_data: &str) -> String {
    if base64_data.len() <= KITTY_CHUNK_SIZE {
        return format!("\x1b_G{params};{base64_data}\x1b\\");
    }
    let mut out = String::with_capacity(base64_data.len() + base64_data.len() / 256 + 64);
    let mut offset = 0;
    while offset < base64_data.len() {
        let end = (offset + KITTY_CHUNK_SIZE).min(base64_data.len());
        // Base64 is ASCII: every byte offset is a char boundary.
        let chunk = &base64_data[offset..end];
        if offset == 0 {
            let _ = write!(out, "\x1b_G{params},m=1;{chunk}\x1b\\");
        } else if end == base64_data.len() {
            let _ = write!(out, "\x1b_Gm=0;{chunk}\x1b\\");
        } else {
            let _ = write!(out, "\x1b_Gm=1;{chunk}\x1b\\");
        }
        offset = end;
    }
    out
}

/// Transmit and place a PNG through the kitty graphics protocol (TS
/// `encodeKitty`, byte-identical): `a=T,f=100,q=2` plus the options.
pub fn encode_kitty(base64_data: &str, options: &KittyOptions) -> String {
    let mut params = vec!["a=T".to_string(), "f=100".to_string(), "q=2".to_string()];
    if !options.move_cursor {
        params.push("C=1".to_string());
    }
    if let Some(columns) = options.columns.filter(|&value| value > 0) {
        params.push(format!("c={columns}"));
    }
    if let Some(rows) = options.rows.filter(|&value| value > 0) {
        params.push(format!("r={rows}"));
    }
    if let Some(id) = options.image_id.filter(|&value| value > 0) {
        params.push(format!("i={id}"));
    }
    kitty_chunked(&params.join(","), base64_data)
}

/// Transmit a PNG under `image_id` without placing it (`a=t`): the image
/// stays stored terminal-side, so every later move is a [`kitty_place`]
/// instead of a re-send. Not in TS (its inline renderer re-sent `a=T`
/// whenever the row repainted).
pub fn kitty_transmit(base64_data: &str, image_id: u32) -> String {
    kitty_chunked(&format!("a=t,f=100,i={image_id},q=2"), base64_data)
}

/// The source rectangle of a cropped kitty placement, in image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyCrop {
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Place a transmitted image at the cursor (`a=p`) over `columns` x `rows`
/// cells without moving the cursor; `crop` shows only a horizontal band of
/// the source (a partly scrolled-out preview).
pub fn kitty_place(image_id: u32, columns: u32, rows: u32, crop: Option<KittyCrop>) -> String {
    let mut out = format!("\x1b_Ga=p,i={image_id},q=2,C=1,c={columns},r={rows}");
    if let Some(crop) = crop {
        let _ = write!(out, ",x=0,y={},w={},h={}", crop.y, crop.width, crop.height);
    }
    out.push_str("\x1b\\");
    out
}

/// Delete every placement of `image_id` but keep its data (lowercase `i`),
/// so the next frame can re-place it without a re-send.
pub fn kitty_delete_placements(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={image_id},q=2\x1b\\")
}

/// Delete a kitty image by id and free its data (TS `deleteKittyImage`).
pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

/// A random kitty image id in `[1, 0xffff_ffff]` (TS `allocateImageId`:
/// random ids avoid collisions with other image writers on the screen).
pub fn allocate_image_id() -> u32 {
    (uuid::Uuid::new_v4().as_u128() as u32).max(1)
}

/// An iTerm2 `width`/`height` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Iterm2Size {
    Cells(u32),
    Auto,
}

impl std::fmt::Display for Iterm2Size {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cells(cells) => write!(f, "{cells}"),
            Self::Auto => f.write_str("auto"),
        }
    }
}

/// Options for [`encode_iterm2`] (TS `encodeITerm2` options).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iterm2Options {
    pub width: Option<Iterm2Size>,
    pub height: Option<Iterm2Size>,
    pub name: Option<String>,
    pub preserve_aspect_ratio: bool,
    pub inline: bool,
}

impl Default for Iterm2Options {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            name: None,
            preserve_aspect_ratio: true,
            inline: true,
        }
    }
}

/// An iTerm2 inline image (TS `encodeITerm2`, byte-identical): the file's
/// own bytes, so PNG, JPEG and GIF need no decoding.
pub fn encode_iterm2(base64_data: &str, options: &Iterm2Options) -> String {
    use base64::Engine;
    let mut params = vec![format!("inline={}", u8::from(options.inline))];
    if let Some(width) = options.width {
        params.push(format!("width={width}"));
    }
    if let Some(height) = options.height {
        params.push(format!("height={height}"));
    }
    if let Some(name) = options.name.as_deref().filter(|name| !name.is_empty()) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(name);
        params.push(format!("name={encoded}"));
    }
    if !options.preserve_aspect_ratio {
        params.push("preserveAspectRatio=0".to_string());
    }
    format!("\x1b]1337;File={}:{base64_data}\x07", params.join(";"))
}

/// The rows an image scaled to `target_width_cells` covers (TS
/// `calculateImageRows`): at least one.
pub fn calculate_image_rows(
    image: ImageDimensions,
    target_width_cells: u32,
    cell: CellDimensions,
) -> u32 {
    let target_width_px = f64::from(target_width_cells) * f64::from(cell.width_px);
    let scale = target_width_px / f64::from(image.width_px.max(1));
    let scaled_height_px = f64::from(image.height_px) * scale;
    let rows = (scaled_height_px / f64::from(cell.height_px.max(1))).ceil();
    (rows as u32).max(1)
}

/// Image pixel dimensions (TS `ImageDimensions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

const KITTY_PREFIX: &str = "\x1b_G";
const ITERM2_PREFIX: &str = "\x1b]1337;File=";

/// Whether a rendered row carries an image placement sequence (TS
/// `isImageLine`).
pub fn is_image_line(line: &str) -> bool {
    line.contains(KITTY_PREFIX) || line.contains(ITERM2_PREFIX)
}

fn png_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 24 {
        return None;
    }
    if bytes[0] != 0x89 || bytes[1] != 0x50 || bytes[2] != 0x4e || bytes[3] != 0x47 {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some(ImageDimensions {
        width_px: width,
        height_px: height,
    })
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    use std::cmp::min;
    if bytes.len() < 2 {
        return None;
    }
    if bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut offset = 2usize;
    while offset + 9 < bytes.len() {
        if bytes[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = bytes[offset + 1];
        if (0xc0..=0xc2).contains(&marker) {
            let height = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]);
            let width = u16::from_be_bytes([bytes[offset + 7], bytes[offset + 8]]);
            return Some(ImageDimensions {
                width_px: u32::from(width),
                height_px: u32::from(height),
            });
        }
        if offset + 3 >= bytes.len() {
            return None;
        }
        let length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]);
        if length < 2 {
            return None;
        }
        offset = min(offset + 2 + length as usize, bytes.len());
    }
    None
}

fn gif_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 10 {
        return None;
    }
    let signature = &bytes[..6];
    if signature != b"GIF87a" && signature != b"GIF89a" {
        return None;
    }
    let width = u16::from_le_bytes([bytes[6], bytes[7]]);
    let height = u16::from_le_bytes([bytes[8], bytes[9]]);
    Some(ImageDimensions {
        width_px: u32::from(width),
        height_px: u32::from(height),
    })
}

fn webp_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 30 {
        return None;
    }
    if &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let chunk = &bytes[12..16];
    if chunk == b"VP8 " {
        let width = u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3fff;
        let height = u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3fff;
        Some(ImageDimensions {
            width_px: u32::from(width),
            height_px: u32::from(height),
        })
    } else if chunk == b"VP8L" {
        if bytes.len() < 25 {
            return None;
        }
        let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
        let width = (bits & 0x3fff) + 1;
        let height = ((bits >> 14) & 0x3fff) + 1;
        Some(ImageDimensions {
            width_px: width,
            height_px: height,
        })
    } else if chunk == b"VP8X" {
        let width =
            (u32::from(bytes[24]) | u32::from(bytes[25]) << 8 | u32::from(bytes[26]) << 16) + 1;
        let height =
            (u32::from(bytes[27]) | u32::from(bytes[28]) << 8 | u32::from(bytes[29]) << 16) + 1;
        Some(ImageDimensions {
            width_px: width,
            height_px: height,
        })
    } else {
        None
    }
}

/// The bounded-prefix decode budget for [`get_image_dimensions_prefix`]:
/// a payload whose header spills past the budget reports `None`.
pub const IMAGE_DIMENSIONS_PREFIX_BYTES: usize = 1024;

/// Read an image's pixel dimensions from a BOUNDED PREFIX of its base64
/// payload (image-heavy tool results must never be decoded whole for a
/// metadata row). `None` for unsupported mime types, payloads that do
/// not decode at the quantum-aligned prefix, or headers that spill past
/// [`IMAGE_DIMENSIONS_PREFIX_BYTES`].
pub fn get_image_dimensions_prefix(
    base64_data: &str,
    mime_type: &str,
    max_decoded_bytes: usize,
) -> Option<ImageDimensions> {
    use base64::Engine;
    // The bounded window comes first and the trim stays INSIDE it: a
    // payload padded with megabytes of trailing whitespace never pays a
    // full-suffix scan.
    let window = base64_data.trim_start();
    let take = (max_decoded_bytes.div_ceil(3) * 4).min(window.len());
    let window = window.get(..take)?.trim_end();
    // Keep the prefix at a multiple of 4 base64 characters so the slice
    // decodes as a complete unpadded sequence.
    let aligned = window.len() - window.len() % 4;
    let prefix = window.get(..aligned)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(prefix)
        .ok()?;
    match mime_type {
        "image/png" => png_dimensions(&bytes),
        "image/jpeg" => jpeg_dimensions(&bytes),
        "image/gif" => gif_dimensions(&bytes),
        "image/webp" => webp_dimensions(&bytes),
        _ => None,
    }
}

/// The textual fallback for an image that cannot be displayed (TS
/// `imageFallback`).
pub fn image_fallback(
    mime_type: &str,
    dimensions: Option<ImageDimensions>,
    filename: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(filename) = filename {
        parts.push(filename.to_string());
    }
    parts.push(format!("[{mime_type}]"));
    if let Some(dimensions) = dimensions {
        parts.push(format!("{}x{}", dimensions.width_px, dimensions.height_px));
    }
    format!("[Image: {}]", parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn tiny_png(width: u32, height: u32) -> String {
        let mut bytes = vec![0x89, b'P', b'N', b'G'];
        bytes.extend(vec![0u8; 12]); // length + IHDR tag
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn png_dimensions_parse_from_payload() {
        let data = tiny_png(64, 32);
        assert_eq!(
            get_image_dimensions_prefix(&data, "image/png", IMAGE_DIMENSIONS_PREFIX_BYTES),
            Some(ImageDimensions {
                width_px: 64,
                height_px: 32
            })
        );
        assert_eq!(
            get_image_dimensions_prefix(&data, "image/jpeg", IMAGE_DIMENSIONS_PREFIX_BYTES),
            None
        );
        assert_eq!(
            get_image_dimensions_prefix("!!!", "image/png", IMAGE_DIMENSIONS_PREFIX_BYTES),
            None
        );
    }

    #[test]
    fn jpeg_dimensions_parse_from_sof_marker() {
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
        bytes.extend(*b"JFIF");
        bytes.extend(vec![0u8; 12]); // APP0 body
        bytes.extend([0xff, 0xc0, 0x00, 0x11, 0x08]); // SOF0
        bytes.extend(720u16.to_be_bytes()); // height
        bytes.extend(1080u16.to_be_bytes()); // width
        bytes.extend(vec![0u8; 8]); // SOF payload tail: the scan needs
                                    // `offset + 9 < len`
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert_eq!(
            get_image_dimensions_prefix(&data, "image/jpeg", IMAGE_DIMENSIONS_PREFIX_BYTES),
            Some(ImageDimensions {
                width_px: 1080,
                height_px: 720
            })
        );
    }

    #[test]
    fn gif_and_webp_dimensions_parse() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend(32u16.to_le_bytes());
        gif.extend(16u16.to_le_bytes());
        let data = base64::engine::general_purpose::STANDARD.encode(gif);
        assert_eq!(
            get_image_dimensions_prefix(&data, "image/gif", IMAGE_DIMENSIONS_PREFIX_BYTES),
            Some(ImageDimensions {
                width_px: 32,
                height_px: 16
            })
        );

        let mut webp = b"RIFF".to_vec();
        webp.extend([0x24, 0x00, 0x00, 0x00]); // size
        webp.extend(b"WEBPVP8X");
        webp.extend(vec![0u8; 14]); // VP8X size + flags + canvas minus-1
        webp[24] = 0x63; // width - 1 = 99
        webp[27] = 0x4f; // height - 1 = 79
        let data = base64::engine::general_purpose::STANDARD.encode(webp);
        assert_eq!(
            get_image_dimensions_prefix(&data, "image/webp", IMAGE_DIMENSIONS_PREFIX_BYTES),
            Some(ImageDimensions {
                width_px: 100,
                height_px: 80
            })
        );
    }

    #[test]
    fn the_prefix_read_stays_bounded_around_whitespace_padding() {
        let padded = format!("{}{}", tiny_png(64, 32), " ".repeat(1 << 20));
        assert_eq!(
            get_image_dimensions_prefix(&padded, "image/png", IMAGE_DIMENSIONS_PREFIX_BYTES),
            Some(ImageDimensions {
                width_px: 64,
                height_px: 32
            })
        );
        let newline = format!("{}\n", tiny_png(64, 32));
        assert_eq!(
            get_image_dimensions_prefix(&newline, "image/png", IMAGE_DIMENSIONS_PREFIX_BYTES),
            Some(ImageDimensions {
                width_px: 64,
                height_px: 32
            })
        );
        assert_eq!(
            get_image_dimensions_prefix(
                &" ".repeat(4096),
                "image/png",
                IMAGE_DIMENSIONS_PREFIX_BYTES
            ),
            None
        );
    }

    #[test]
    fn the_prefix_read_never_touches_the_payload_past_the_budget() {
        // A payload whose prefix decodes but whose tail (past the
        // budget) is invalid base64 still reports its dimensions.
        let header = tiny_png(640, 480);
        let poisoned = format!("{header}{}{}", "A".repeat(4096), "!".repeat(64));
        assert_eq!(
            get_image_dimensions_prefix(&poisoned, "image/png", IMAGE_DIMENSIONS_PREFIX_BYTES),
            Some(ImageDimensions {
                width_px: 640,
                height_px: 480
            })
        );
        // A header that spills past the budget reports None: dims sit
        // at byte 16 here, so a 12-byte budget cannot see them.
        assert_eq!(get_image_dimensions_prefix(&header, "image/png", 12), None);
    }

    #[test]
    fn fallback_text_shapes() {
        assert_eq!(
            image_fallback("image/png", None, None),
            "[Image: [image/png]]"
        );
        assert_eq!(
            image_fallback(
                "image/png",
                Some(ImageDimensions {
                    width_px: 800,
                    height_px: 600
                }),
                None
            ),
            "[Image: [image/png] 800x600]"
        );
        assert_eq!(
            image_fallback(
                "image/png",
                Some(ImageDimensions {
                    width_px: 8,
                    height_px: 6
                }),
                Some("shot.png")
            ),
            "[Image: shot.png [image/png] 8x6]"
        );
    }

    #[test]
    fn is_image_line_detects_both_protocols() {
        assert!(is_image_line("\x1b_Ga=T;QUJD\x1b\\"));
        assert!(is_image_line("\x1b[3A\x1b_Ga=T;QUJD\x1b\\"));
        assert!(is_image_line("\x1b]1337;File=inline=1:QQ\x07"));
        assert!(!is_image_line("plain row"));
    }
}

#[cfg(test)]
mod ts_parity_tests;
