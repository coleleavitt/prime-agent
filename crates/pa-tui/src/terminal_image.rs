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
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// How image escapes reach the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageTransport {
    /// Straight to the terminal (TS's only path).
    Direct,
    /// Through tmux's passthrough (`DCS tmux; … ST`, see [`tmux`]): kitty
    /// images become unicode placeholders tmux owns as text; iTerm2 images
    /// address the client screen at the pane's origin.
    Tmux { origin_row: u16, origin_column: u16 },
}

/// The process's image terminal: the protocol and how its escapes travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageTerminal {
    pub protocol: ImageProtocol,
    pub transport: ImageTransport,
}

impl ImageTerminal {
    #[must_use]
    pub fn direct(protocol: ImageProtocol) -> Self {
        Self {
            protocol,
            transport: ImageTransport::Direct,
        }
    }

    /// Whether kitty images are unicode placeholders drawn by the cells.
    #[must_use]
    pub fn placeholders(self) -> bool {
        self.protocol == ImageProtocol::Kitty && self.tmux()
    }

    /// Whether escapes ride tmux's passthrough.
    #[must_use]
    pub fn tmux(self) -> bool {
        matches!(self.transport, ImageTransport::Tmux { .. })
    }
}

thread_local! {
    /// A test's image-terminal override.
    static PROTOCOL_OVERRIDE: Cell<Option<ForcedTerminal>> = const { Cell::new(None) };
    /// A test's cell-size override.
    static CELL_OVERRIDE: Cell<Option<CellDimensions>> = const { Cell::new(None) };
}

/// The environment's answer (TS `getCapabilities().images`), read once.
fn env_image_terminal() -> Option<ImageTerminal> {
    static DETECTED: OnceLock<Option<ImageTerminal>> = OnceLock::new();
    *DETECTED.get_or_init(|| {
        detect_image_protocol(|name| std::env::var(name).ok()).map(ImageTerminal::direct)
    })
}

/// A probe's answer when the environment had none (see
/// [`start_image_detection`]).
static PROBED: std::sync::Mutex<Option<ImageTerminal>> = std::sync::Mutex::new(None);

fn probed() -> Option<ImageTerminal> {
    *PROBED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record a probe's answer, size the cells for it, and wake the session
/// loop so the next frame lays the previews out for it.
fn set_probed(terminal: ImageTerminal) {
    *PROBED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(terminal);
    refresh_cell_dimensions();
    crate::inline_image::notify_terminal_changed();
}

/// The process's image terminal: the environment's answer (TS parity),
/// else what a probe found.
pub fn image_terminal() -> Option<ImageTerminal> {
    if let Some(ForcedTerminal(forced)) = PROTOCOL_OVERRIDE.with(Cell::get) {
        return forced;
    }
    env_image_terminal().or_else(probed)
}

/// The process's image protocol (TS `getCapabilities().images`).
pub fn image_protocol() -> Option<ImageProtocol> {
    image_terminal().map(|terminal| terminal.protocol)
}

/// Which probe can find an image terminal the environment does not name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeRoute {
    /// The environment answered, or nothing can be asked.
    None,
    /// Ask the tmux server about its client ([`tmux`]).
    Tmux,
    /// Ask the terminal itself: the kitty graphics query, concluded by the
    /// keyboard probe's DA1. An ssh session hides the emulator's variables
    /// (openssh forwards no environment by default — `SendEnv` and
    /// `AcceptEnv` start empty — and sends only `TERM`, in the pty
    /// request), so `KITTY_WINDOW_ID`/`TERM_PROGRAM` never arrive.
    GraphicsQuery,
}

/// The probe for this environment, after TS's detection said none. tmux is
/// asked, never the terminal through it (tmux answers DA1 itself and drops
/// the graphics reply); screen and zellij are never asked (screen can take
/// an APC as a window title); a `dumb`/`linux` console or a non-tty stdout
/// has no reply to give.
pub(crate) fn probe_route(env: impl Fn(&str) -> Option<String>, stdout_is_tty: bool) -> ProbeRoute {
    let var = |name: &str| env(name).filter(|value| !value.is_empty());
    if detect_image_protocol(&env).is_some() || !stdout_is_tty {
        return ProbeRoute::None;
    }
    if var("TMUX").is_some() {
        return ProbeRoute::Tmux;
    }
    let term = var("TERM").unwrap_or_default().to_lowercase();
    if term.starts_with("tmux")
        || term.starts_with("screen")
        || var("STY").is_some()
        || var("ZELLIJ").is_some()
        || matches!(term.as_str(), "" | "dumb" | "linux")
    {
        return ProbeRoute::None;
    }
    ProbeRoute::GraphicsQuery
}

/// Start the probe for a terminal the environment does not name, once per
/// process, off the paint path; until it answers the previews keep their
/// textual fallback. Runs at the first surface's mount, before the
/// keyboard probe that carries the graphics query.
pub fn start_image_detection() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        use std::io::IsTerminal;
        let route = probe_route(
            |name| std::env::var(name).ok(),
            std::io::stdout().is_terminal(),
        );
        match route {
            ProbeRoute::None => {}
            ProbeRoute::Tmux => {
                let _ = std::thread::Builder::new()
                    .name("pa-image-tmux-probe".to_string())
                    .spawn(|| {
                        if let Some(terminal) = tmux::probe_tmux_client()
                            .and_then(|client| tmux::tmux_image_terminal(&client))
                        {
                            set_probed(terminal);
                        }
                    });
            }
            ProbeRoute::GraphicsQuery => request_graphics_query(),
        }
    });
}

#[cfg(unix)]
fn request_graphics_query() {
    crossterm::terminal::request_kitty_graphics_query();
}

/// The keyboard probe (and with it the query) is unix-only.
#[cfg(not(unix))]
fn request_graphics_query() {}

/// Take the graphics query's verdict once it landed (the keyboard probe's
/// settle and the input reader's late-reply path call this): an `OK`
/// makes the terminal a direct kitty one. Transmission is always direct
/// (`t=d`, the default every command here leaves implicit): file and
/// shared-memory media would name paths on the wrong side of an ssh hop.
pub(crate) fn take_graphics_query_reply() {
    if graphics_query_verdict() == Some(true) {
        set_probed(ImageTerminal::direct(ImageProtocol::Kitty));
    }
}

#[cfg(unix)]
fn graphics_query_verdict() -> Option<bool> {
    crossterm::event::take_kitty_graphics_reply()
}

#[cfg(not(unix))]
fn graphics_query_verdict() -> Option<bool> {
    None
}

/// A test-forced terminal (`None`: the fallback terminal).
#[derive(Debug, Clone, Copy)]
struct ForcedTerminal(Option<ImageTerminal>);

/// Force this thread's image protocol (`None`: a terminal without one).
#[cfg(test)]
pub(crate) fn set_image_protocol_override(protocol: Option<ImageProtocol>) {
    set_image_terminal_override(protocol.map(ImageTerminal::direct));
}

/// Force this thread's image terminal (`None`: a terminal without one).
#[cfg(test)]
pub(crate) fn set_image_terminal_override(terminal: Option<ImageTerminal>) {
    PROTOCOL_OVERRIDE.with(|cell| cell.set(Some(ForcedTerminal(terminal))));
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

/// A resize under tmux can move the pane (a split, a layout change): an
/// iTerm2 placement addresses the client screen, so the pane's origin is
/// asked again, off the paint path. Every other terminal only re-reads the
/// cell size.
pub fn terminal_resized() {
    static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    refresh_cell_dimensions();
    let Some(current) = probed() else {
        return;
    };
    if current.protocol != ImageProtocol::Iterm2
        || !current.tmux()
        || IN_FLIGHT.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("pa-image-tmux-probe".to_string())
        .spawn(move || {
            let answer =
                tmux::probe_tmux_client().and_then(|client| tmux::tmux_image_terminal(&client));
            if let Some(terminal) = answer.filter(|terminal| *terminal != current) {
                *PROBED
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(terminal);
                crate::inline_image::notify_terminal_changed();
            }
            IN_FLIGHT.store(false, Ordering::SeqCst);
        });
    if spawned.is_err() {
        IN_FLIGHT.store(false, Ordering::SeqCst);
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
/// The production path is [`encode_kitty_with_format`] (the same bytes for
/// a PNG); this form stays for the TS byte-parity goldens.
#[cfg(test)]
pub fn encode_kitty(base64_data: &str, options: &KittyOptions) -> String {
    encode_kitty_with_format(base64_data, "f=100", options)
}

/// [`encode_kitty`] for a payload whose format keys are not PNG's `f=100`
/// (the raw RGBA of a decoded GIF or WebP: `f=32,s=…,v=…,o=z`), in the
/// same position.
pub(crate) fn encode_kitty_with_format(
    base64_data: &str,
    format_keys: &str,
    options: &KittyOptions,
) -> String {
    let mut params = vec![
        "a=T".to_string(),
        format_keys.to_string(),
        "q=2".to_string(),
    ];
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
#[cfg(test)]
pub fn kitty_transmit(base64_data: &str, image_id: u32) -> String {
    kitty_transmit_with_format(base64_data, "f=100", image_id)
}

/// [`kitty_transmit`] with the payload's own format keys.
pub(crate) fn kitty_transmit_with_format(
    base64_data: &str,
    format_keys: &str,
    image_id: u32,
) -> String {
    kitty_chunked(&format!("a=t,{format_keys},i={image_id},q=2"), base64_data)
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
    use base64::Engine;

    use super::*;

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

    /// The env matrix after TS's detection: direct terminals need no
    /// probe; ssh (only `TERM` crosses) asks the terminal; tmux — with or
    /// without ssh underneath — asks the tmux server; screen, zellij, a
    /// console, and a non-tty never ask.
    #[test]
    fn the_probe_route_follows_the_environment() {
        let route = |vars: &[(&str, &str)], tty: bool| {
            probe_route(
                |name| {
                    vars.iter()
                        .find(|(key, _)| *key == name)
                        .map(|(_, value)| (*value).to_string())
                },
                tty,
            )
        };
        let ssh = [
            ("SSH_CONNECTION", "192.0.2.1 50000 192.0.2.2 22"),
            ("SSH_TTY", "/dev/pts/3"),
        ];
        let with = |extra: &[(&'static str, &'static str)]| -> Vec<(&'static str, &'static str)> {
            ssh.iter().chain(extra).copied().collect()
        };
        // Direct kitty: TS's detection answers.
        assert_eq!(
            route(&[("KITTY_WINDOW_ID", "1"), ("TERM", "xterm-kitty")], true),
            ProbeRoute::None
        );
        // kitty over ssh: only TERM crossed.
        assert_eq!(
            route(&with(&[("TERM", "xterm-kitty")]), true),
            ProbeRoute::GraphicsQuery
        );
        // Any other terminal over ssh is asked too (the reply decides).
        assert_eq!(
            route(&with(&[("TERM", "xterm-256color")]), true),
            ProbeRoute::GraphicsQuery
        );
        // Ghostty's TERM names it: TS's detection already answers.
        assert_eq!(
            route(&with(&[("TERM", "xterm-ghostty")]), true),
            ProbeRoute::None
        );
        // tmux, local or behind ssh: ask the server, not the terminal.
        assert_eq!(
            route(
                &[
                    ("TMUX", "/tmp/tmux-1000/default,1,0"),
                    ("TERM", "tmux-256color")
                ],
                true
            ),
            ProbeRoute::Tmux
        );
        assert_eq!(
            route(
                &with(&[
                    ("TMUX", "/tmp/tmux-1000/default,1,0"),
                    ("TERM", "tmux-256color")
                ]),
                true
            ),
            ProbeRoute::Tmux
        );
        // A stale KITTY_WINDOW_ID inside tmux does not skip the tmux probe.
        assert_eq!(
            route(&[("TMUX", "/tmp/t,1,0"), ("KITTY_WINDOW_ID", "1")], true),
            ProbeRoute::Tmux
        );
        // ssh out of a tmux pane (TERM crossed, TMUX did not), screen,
        // zellij, consoles, and pipes: never asked.
        for vars in [
            with(&[("TERM", "tmux-256color")]),
            with(&[("TERM", "screen-256color")]),
            with(&[("TERM", "xterm-256color"), ("STY", "1.pts-0.host")]),
            with(&[("TERM", "xterm-256color"), ("ZELLIJ", "0")]),
            with(&[("TERM", "linux")]),
            with(&[("TERM", "dumb")]),
            with(&[]),
        ] {
            assert_eq!(route(&vars, true), ProbeRoute::None, "{vars:?}");
        }
        assert_eq!(
            route(&with(&[("TERM", "xterm-kitty")]), false),
            ProbeRoute::None
        );
    }

    /// Every kitty command the port writes transmits directly (`t=d`, the
    /// protocol default, so no `t=` key at all): a file (`t=f`/`t=t`) or
    /// shared-memory (`t=s`) medium names a path on the wrong side of an
    /// ssh hop.
    #[test]
    fn kitty_transmission_is_always_direct() {
        use kitty_graphics::{Command, tmux_write_with_payload};
        let payload = "QUJD".repeat(3000);
        let written = [
            encode_kitty(&payload, &KittyOptions::default()),
            kitty_transmit(&payload, 7),
            kitty_place(7, 10, 5, None),
            tmux_write_with_payload(
                &Command::new()
                    .key(b'a', 'T')
                    .key(b'q', 2)
                    .key(b'f', 100)
                    .key(b'U', 1)
                    .key(b'i', 7),
                &payload,
            ),
        ];
        for escapes in written {
            let keys: Vec<&str> = escapes
                .split("_G")
                .skip(1)
                .flat_map(|command| command.split(';').next().unwrap_or("").split(','))
                .collect();
            assert!(!keys.is_empty());
            assert!(keys.iter().all(|key| !key.starts_with("t=")), "{keys:?}");
        }
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

pub(crate) mod kitty_graphics;
pub(crate) mod tmux;
