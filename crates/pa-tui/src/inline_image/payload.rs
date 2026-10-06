//! Preview bytes for the painter, prepared off the paint path. iTerm2 takes
//! the file bytes as they are (PNG, JPEG, GIF — an animated GIF animates
//! there), and a WebP as a PNG transcode (iTerm2 decodes through macOS,
//! whose WebP support depends on the OS release). kitty takes a PNG as it
//! is (`f=100`); a JPEG is decoded and re-encoded as PNG; a GIF or WebP is
//! decoded to its FIRST FRAME only (an animated preview shows as a still on
//! kitty) and sent as zlib-compressed RGBA (`f=32,o=z`) — no PNG re-encode.
//! Every transcode runs once, on a background thread; the paint path only
//! reads the caches.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

/// How kitty's payload bytes are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KittyFormat {
    /// A PNG file (`f=100`).
    Png,
    /// zlib-compressed 8-bit RGBA rows (`f=32,o=z` with `s`/`v`).
    Rgba,
}

/// kitty's ready payload: base64 bytes plus the pixel size crops are cut in
/// (and an RGBA payload declares).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct KittyPayload {
    pub(crate) base64: Arc<str>,
    pub(crate) width_px: u32,
    pub(crate) height_px: u32,
    pub(crate) format: KittyFormat,
}

impl KittyPayload {
    /// The transmit's format keys, in the TS encoder's `f=` position.
    pub(crate) fn format_keys(&self) -> String {
        match self.format {
            KittyFormat::Png => "f=100".to_string(),
            KittyFormat::Rgba => format!("f=32,s={},v={},o=z", self.width_px, self.height_px),
        }
    }
}

/// What the painter can do with an image this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KittyPayloadState {
    Ready(Arc<KittyPayload>),
    /// The transcode is still running: the reserved block stays blank and
    /// the frame after [`payload_ready`] places it.
    Pending,
    Unavailable,
}

/// Where the painter reads image bytes from. The process implementation is
/// [`GlobalSource`]; tests substitute fixed payloads. Implementations must
/// return immediately: they run on the paint path.
pub(crate) trait PayloadSource {
    /// iTerm2's base64 file bytes for the image (`None` while a transcode
    /// runs, or for an unknown image).
    fn file(&self, key: u64) -> Option<Arc<str>>;
    /// kitty's payload for the image.
    fn kitty(&self, key: u64) -> KittyPayloadState;
}

struct Registered {
    data: Weak<str>,
    mime_type: String,
}

/// Which protocol a prepared payload is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Target {
    Kitty,
    Iterm2,
}

enum Prepared {
    Pending,
    Ready(Arc<KittyPayload>),
    Failed,
}

/// Rows register their image when they decode; the weak handle lets the
/// bytes go with the row.
static REGISTRY: LazyLock<Mutex<HashMap<u64, Registered>>> = LazyLock::new(Mutex::default);
/// Prepared payloads by image key and protocol.
static PREPARED: LazyLock<Mutex<HashMap<(u64, Target), Prepared>>> = LazyLock::new(Mutex::default);
/// Bumped per failed transcode: the failed row re-lays out as its fallback.
static FAILURES: AtomicU64 = AtomicU64::new(0);
/// Woken when a transcode settles, so the session loop repaints.
static READY: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// The most payloads kept: a resumed session full of previews keeps only
/// the most recent ones resident (an evicted one re-prepares).
const MAX_PREPARED: usize = 16;
/// Transcoded previews shrink by an integer factor until no edge exceeds
/// this many pixels (the kernel's previews reach 1600).
const MAX_TRANSCODED_EDGE: u32 = 1024;
/// The largest JPEG the transcode decodes.
const MAX_DECODED_EDGE: usize = 4096;
/// The most bytes one decoded GIF or WebP frame may take (4096 x 4096 RGBA).
const MAX_DECODED_BYTES: u64 = 4096 * 4096 * 4;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) fn register(key: u64, data: &Arc<str>, mime_type: &str) {
    let mut registry = lock(&REGISTRY);
    if registry.len() >= 64 {
        registry.retain(|_, entry| entry.data.strong_count() > 0);
    }
    registry.insert(
        key,
        Registered {
            data: Arc::downgrade(data),
            mime_type: mime_type.to_string(),
        },
    );
}

pub(super) fn failed(key: u64) -> bool {
    lock(&PREPARED)
        .iter()
        .any(|((prepared, _), state)| *prepared == key && matches!(state, Prepared::Failed))
}

pub(super) fn failure_epoch() -> u64 {
    FAILURES.load(Ordering::Relaxed)
}

/// Resolves once a background transcode settled (ready or failed), or the
/// image terminal changed.
pub(crate) async fn payload_ready() {
    READY.notified().await;
}

pub(super) fn notify_ready() {
    READY.notify_one();
}

/// A transcode from a registered image's base64 bytes.
type Transcode = fn(&str) -> Result<KittyPayload, TranscodeError>;

/// The process payload source.
pub(crate) struct GlobalSource;

impl GlobalSource {
    /// The prepared payload for `key` and `target`, starting `transcode`
    /// when nothing is prepared yet.
    fn prepared(
        key: u64,
        target: Target,
        data: Arc<str>,
        transcode: Transcode,
    ) -> KittyPayloadState {
        let mut prepared = lock(&PREPARED);
        match prepared.get(&(key, target)) {
            Some(Prepared::Ready(payload)) => return KittyPayloadState::Ready(payload.clone()),
            Some(Prepared::Pending) => return KittyPayloadState::Pending,
            Some(Prepared::Failed) => return KittyPayloadState::Unavailable,
            None => {}
        }
        if prepared.len() >= MAX_PREPARED {
            prepared.retain(|_, state| !matches!(state, Prepared::Ready(_)));
        }
        prepared.insert((key, target), Prepared::Pending);
        spawn_transcode(key, target, data, transcode);
        KittyPayloadState::Pending
    }

    fn registered(key: u64) -> Option<(Arc<str>, String)> {
        lock(&REGISTRY)
            .get(&key)
            .and_then(|entry| Some((entry.data.upgrade()?, entry.mime_type.clone())))
    }
}

impl PayloadSource for GlobalSource {
    fn file(&self, key: u64) -> Option<Arc<str>> {
        let (data, mime_type) = Self::registered(key)?;
        if mime_type != "image/webp" {
            return Some(data);
        }
        match Self::prepared(key, Target::Iterm2, data, webp_base64_to_png) {
            KittyPayloadState::Ready(payload) => Some(payload.base64.clone()),
            KittyPayloadState::Pending | KittyPayloadState::Unavailable => None,
        }
    }

    fn kitty(&self, key: u64) -> KittyPayloadState {
        let Some((data, mime_type)) = Self::registered(key) else {
            return KittyPayloadState::Unavailable;
        };
        let transcode: Transcode = match mime_type.as_str() {
            "image/png" => {
                let dims = crate::terminal_image::get_image_dimensions_prefix(
                    &data,
                    "image/png",
                    crate::terminal_image::IMAGE_DIMENSIONS_PREFIX_BYTES,
                );
                return dims.map_or(KittyPayloadState::Unavailable, |dims| {
                    KittyPayloadState::Ready(Arc::new(KittyPayload {
                        base64: data,
                        width_px: dims.width_px,
                        height_px: dims.height_px,
                        format: KittyFormat::Png,
                    }))
                });
            }
            "image/jpeg" => jpeg_base64_to_png,
            "image/gif" => gif_base64_to_rgba,
            "image/webp" => webp_base64_to_rgba,
            _ => return KittyPayloadState::Unavailable,
        };
        Self::prepared(key, Target::Kitty, data, transcode)
    }
}

fn spawn_transcode(key: u64, target: Target, data: Arc<str>, transcode: Transcode) {
    let spawned = std::thread::Builder::new()
        .name("pa-image-transcode".to_string())
        .spawn(move || {
            let outcome = transcode(&data);
            settle(key, target, outcome);
        });
    if spawned.is_err() {
        settle(key, target, Err(TranscodeError::Spawn));
    }
}

fn settle(key: u64, target: Target, outcome: Result<KittyPayload, TranscodeError>) {
    // A failed row re-lays out as its textual fallback.
    let state = if let Ok(payload) = outcome {
        Prepared::Ready(Arc::new(payload))
    } else {
        FAILURES.fetch_add(1, Ordering::Relaxed);
        Prepared::Failed
    };
    lock(&PREPARED).insert((key, target), state);
    READY.notify_one();
}

/// Why a preview could not become the terminal's payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TranscodeError {
    Base64,
    Decode(String),
    /// The decoder produced fewer pixels than the header promised.
    Truncated,
    Spawn,
}

impl std::fmt::Display for TranscodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Base64 => f.write_str("the preview is not base64"),
            Self::Decode(reason) => write!(f, "the image does not decode: {reason}"),
            Self::Truncated => f.write_str("the decoded image is truncated"),
            Self::Spawn => f.write_str("the transcode thread did not start"),
        }
    }
}

impl std::error::Error for TranscodeError {}

fn decode_base64(data: &str) -> Result<Vec<u8>, TranscodeError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|_| TranscodeError::Base64)
}

fn encode_base64(bytes: &[u8]) -> Arc<str> {
    use base64::Engine;
    Arc::from(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// The integer factor that brings the longer edge under
/// [`MAX_TRANSCODED_EDGE`].
fn shrink_factor(width: u32, height: u32) -> u32 {
    width.max(height).div_ceil(MAX_TRANSCODED_EDGE).max(1)
}

/// Decode a base64 JPEG, shrink it under [`MAX_TRANSCODED_EDGE`], and
/// re-encode it as a base64 PNG.
pub(crate) fn jpeg_base64_to_png(data: &str) -> Result<KittyPayload, TranscodeError> {
    let jpeg = decode_base64(data)?;
    let options = zune_jpeg::zune_core::options::DecoderOptions::default()
        .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGB)
        .set_max_width(MAX_DECODED_EDGE)
        .set_max_height(MAX_DECODED_EDGE);
    let mut decoder =
        zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(jpeg.as_slice()), options);
    let rgb = decoder
        .decode()
        .map_err(|error| TranscodeError::Decode(error.to_string()))?;
    let info = decoder
        .info()
        .ok_or_else(|| TranscodeError::Decode("no header".to_string()))?;
    let (width, height) = (u32::from(info.width), u32::from(info.height));
    if width == 0 || height == 0 || rgb.len() < width as usize * height as usize * 3 {
        return Err(TranscodeError::Truncated);
    }
    let (rgb, width, height) = shrink_rgb(&rgb, width, height, shrink_factor(width, height));
    Ok(KittyPayload {
        base64: encode_base64(&encode_png_rgb(&rgb, width, height)),
        width_px: width,
        height_px: height,
        format: KittyFormat::Png,
    })
}

/// A decoded frame: 8-bit RGBA rows.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Rgba {
    pub(crate) pixels: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// A GIF's first frame on its logical screen (transparent where the frame
/// does not cover it). Later frames of an animation are never read.
pub(crate) fn decode_gif_first_frame(bytes: &[u8]) -> Result<Rgba, TranscodeError> {
    let decode_error = |error: gif::DecodingError| TranscodeError::Decode(error.to_string());
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    if let Some(limit) = std::num::NonZeroU64::new(MAX_DECODED_BYTES) {
        options.set_memory_limit(gif::MemoryLimit::Bytes(limit));
    }
    let mut decoder = options
        .read_info(std::io::Cursor::new(bytes))
        .map_err(decode_error)?;
    let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) * 4 > MAX_DECODED_BYTES {
        return Err(TranscodeError::Decode(format!("a {width}x{height} screen")));
    }
    let frame = decoder
        .read_next_frame()
        .map_err(decode_error)?
        .ok_or_else(|| TranscodeError::Decode("no frame".to_string()))?;
    let (left, top) = (u32::from(frame.left), u32::from(frame.top));
    let (frame_width, frame_height) = (u32::from(frame.width), u32::from(frame.height));
    if frame.buffer.len() < frame_width as usize * frame_height as usize * 4 {
        return Err(TranscodeError::Truncated);
    }
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    for y in 0..frame_height.min(height.saturating_sub(top)) {
        let columns = frame_width.min(width.saturating_sub(left)) as usize * 4;
        let from = y as usize * frame_width as usize * 4;
        let to = ((top + y) as usize * width as usize + left as usize) * 4;
        pixels[to..to + columns].copy_from_slice(&frame.buffer[from..from + columns]);
    }
    Ok(Rgba {
        pixels,
        width,
        height,
    })
}

/// A WebP's image (an animated WebP's first frame), as RGBA.
pub(crate) fn decode_webp_first_frame(bytes: &[u8]) -> Result<Rgba, TranscodeError> {
    let decode_error = |error: image_webp::DecodingError| TranscodeError::Decode(error.to_string());
    let mut decoder =
        image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).map_err(decode_error)?;
    decoder.set_memory_limit(MAX_DECODED_BYTES as usize);
    let (width, height) = decoder.dimensions();
    let size = decoder
        .output_buffer_size()
        .filter(|size| *size as u64 <= MAX_DECODED_BYTES && width > 0 && height > 0)
        .ok_or_else(|| TranscodeError::Decode(format!("a {width}x{height} image")))?;
    let mut buffer = vec![0u8; size];
    decoder.read_image(&mut buffer).map_err(decode_error)?;
    let pixels = if decoder.has_alpha() {
        buffer
    } else {
        buffer
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|&[r, g, b]| [r, g, b, 255])
            .collect()
    };
    Ok(Rgba {
        pixels,
        width,
        height,
    })
}

/// kitty's raw payload for a decoded frame: shrunk under
/// [`MAX_TRANSCODED_EDGE`], zlib-compressed RGBA.
fn rgba_payload(frame: &Rgba) -> KittyPayload {
    let factor = shrink_factor(frame.width, frame.height);
    let (pixels, width, height) =
        shrink_pixels(&frame.pixels, frame.width, frame.height, factor, 4);
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    // Writing into a Vec cannot fail.
    let _ = zlib.write_all(&pixels);
    KittyPayload {
        base64: encode_base64(&zlib.finish().unwrap_or_default()),
        width_px: width,
        height_px: height,
        format: KittyFormat::Rgba,
    }
}

/// kitty's payload for a GIF preview: its first frame as raw RGBA.
pub(crate) fn gif_base64_to_rgba(data: &str) -> Result<KittyPayload, TranscodeError> {
    Ok(rgba_payload(&decode_gif_first_frame(&decode_base64(
        data,
    )?)?))
}

/// kitty's payload for a WebP preview: its (first) frame as raw RGBA.
pub(crate) fn webp_base64_to_rgba(data: &str) -> Result<KittyPayload, TranscodeError> {
    Ok(rgba_payload(&decode_webp_first_frame(&decode_base64(
        data,
    )?)?))
}

/// iTerm2's payload for a WebP preview: its (first) frame as an RGBA PNG.
pub(crate) fn webp_base64_to_png(data: &str) -> Result<KittyPayload, TranscodeError> {
    let frame = decode_webp_first_frame(&decode_base64(data)?)?;
    let factor = shrink_factor(frame.width, frame.height);
    let (pixels, width, height) =
        shrink_pixels(&frame.pixels, frame.width, frame.height, factor, 4);
    Ok(KittyPayload {
        base64: encode_base64(&encode_png(&pixels, width, height, 4)),
        width_px: width,
        height_px: height,
        format: KittyFormat::Png,
    })
}

/// Box-filter `rgb` down by an integer `factor` (edge pixels average the
/// partial box).
pub(crate) fn shrink_rgb(rgb: &[u8], width: u32, height: u32, factor: u32) -> (Vec<u8>, u32, u32) {
    shrink_pixels(rgb, width, height, factor, 3)
}

/// Box-filter `channels`-byte pixels down by an integer `factor`.
pub(crate) fn shrink_pixels(
    pixels: &[u8],
    width: u32,
    height: u32,
    factor: u32,
    channels: usize,
) -> (Vec<u8>, u32, u32) {
    if factor <= 1 {
        return (
            pixels[..width as usize * height as usize * channels].to_vec(),
            width,
            height,
        );
    }
    let (out_width, out_height) = (width.div_ceil(factor), height.div_ceil(factor));
    let mut out = Vec::with_capacity(out_width as usize * out_height as usize * channels);
    let mut sum = vec![0u32; channels];
    for oy in 0..out_height {
        for ox in 0..out_width {
            sum.fill(0);
            let mut count = 0u32;
            for y in oy * factor..((oy + 1) * factor).min(height) {
                for x in ox * factor..((ox + 1) * factor).min(width) {
                    let at = (y as usize * width as usize + x as usize) * channels;
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += u32::from(pixels[at + channel]);
                    }
                    count += 1;
                }
            }
            out.extend(sum.iter().map(|total| (total / count.max(1)) as u8));
        }
    }
    (out, out_width, out_height)
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// An 8-bit RGB PNG (no filtering, zlib at the fast level).
pub(crate) fn encode_png_rgb(rgb: &[u8], width: u32, height: u32) -> Vec<u8> {
    encode_png(rgb, width, height, 3)
}

/// An 8-bit RGB (`channels` 3) or RGBA (4) PNG.
pub(crate) fn encode_png(pixels: &[u8], width: u32, height: u32, channels: usize) -> Vec<u8> {
    let stride = width as usize * channels;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in pixels.chunks_exact(stride).take(height as usize) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    // Writing into a Vec cannot fail.
    let _ = zlib.write_all(&raw);
    let idat = zlib.finish().unwrap_or_default();
    let mut header = Vec::with_capacity(13);
    header.extend(width.to_be_bytes());
    header.extend(height.to_be_bytes());
    header.extend([8, if channels == 4 { 6 } else { 2 }, 0, 0, 0]);
    let mut png = PNG_SIGNATURE.to_vec();
    png_chunk(&mut png, *b"IHDR", &header);
    png_chunk(&mut png, *b"IDAT", &idat);
    png_chunk(&mut png, *b"IEND", &[]);
    png
}

fn png_chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    png.extend((data.len() as u32).to_be_bytes());
    png.extend(kind);
    png.extend_from_slice(data);
    let mut crc = flate2::Crc::new();
    crc.update(&kind);
    crc.update(data);
    png.extend(crc.sum().to_be_bytes());
}
