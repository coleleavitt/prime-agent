//! Preview bytes for the painter, prepared off the paint path: iTerm2 takes
//! the file bytes as they are, kitty takes a PNG as it is, and a JPEG is
//! decoded and re-encoded as PNG once, on a background thread (kitty's
//! `f=100` accepts PNG only; the kernel re-encodes oversized previews as
//! JPEG). The paint path only reads the caches.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

/// kitty's ready PNG: base64 bytes plus the pixel size crops are cut in.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct KittyPayload {
    pub(crate) base64: Arc<str>,
    pub(crate) width_px: u32,
    pub(crate) height_px: u32,
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
    /// The registered image's base64 file bytes (iTerm2's payload).
    fn file(&self, key: u64) -> Option<Arc<str>>;
    /// kitty's PNG payload for the image.
    fn kitty(&self, key: u64) -> KittyPayloadState;
}

struct Registered {
    data: Weak<str>,
    mime_type: String,
}

enum Prepared {
    Pending,
    Ready(Arc<KittyPayload>),
    Failed,
}

/// Rows register their image when they decode; the weak handle lets the
/// bytes go with the row.
static REGISTRY: LazyLock<Mutex<HashMap<u64, Registered>>> = LazyLock::new(Mutex::default);
/// kitty payloads by image key.
static PREPARED: LazyLock<Mutex<HashMap<u64, Prepared>>> = LazyLock::new(Mutex::default);
/// Bumped per failed transcode: the failed row re-lays out as its fallback.
static FAILURES: AtomicU64 = AtomicU64::new(0);
/// Woken when a transcode settles, so the session loop repaints.
static READY: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// The most kitty payloads kept: a resumed session full of previews keeps
/// only the most recent ones resident (an evicted one re-prepares).
const MAX_PREPARED: usize = 16;
/// Transcoded previews shrink by an integer factor until no edge exceeds
/// this many pixels (the kernel's previews reach 1600).
const MAX_TRANSCODED_EDGE: u32 = 1024;
/// The largest JPEG the transcode decodes.
const MAX_DECODED_EDGE: usize = 4096;

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
    matches!(lock(&PREPARED).get(&key), Some(Prepared::Failed))
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

/// The process payload source.
pub(crate) struct GlobalSource;

impl PayloadSource for GlobalSource {
    fn file(&self, key: u64) -> Option<Arc<str>> {
        lock(&REGISTRY).get(&key)?.data.upgrade()
    }

    fn kitty(&self, key: u64) -> KittyPayloadState {
        let mut prepared = lock(&PREPARED);
        match prepared.get(&key) {
            Some(Prepared::Ready(payload)) => return KittyPayloadState::Ready(payload.clone()),
            Some(Prepared::Pending) => return KittyPayloadState::Pending,
            Some(Prepared::Failed) => return KittyPayloadState::Unavailable,
            None => {}
        }
        let Some((data, mime_type)) = lock(&REGISTRY)
            .get(&key)
            .and_then(|entry| Some((entry.data.upgrade()?, entry.mime_type.clone())))
        else {
            return KittyPayloadState::Unavailable;
        };
        if prepared.len() >= MAX_PREPARED {
            prepared.retain(|_, state| !matches!(state, Prepared::Ready(_)));
        }
        match mime_type.as_str() {
            "image/png" => {
                let dims = crate::terminal_image::get_image_dimensions_prefix(
                    &data,
                    "image/png",
                    crate::terminal_image::IMAGE_DIMENSIONS_PREFIX_BYTES,
                );
                let Some(dims) = dims else {
                    return KittyPayloadState::Unavailable;
                };
                let payload = Arc::new(KittyPayload {
                    base64: data,
                    width_px: dims.width_px,
                    height_px: dims.height_px,
                });
                prepared.insert(key, Prepared::Ready(payload.clone()));
                KittyPayloadState::Ready(payload)
            }
            "image/jpeg" => {
                prepared.insert(key, Prepared::Pending);
                spawn_transcode(key, data);
                KittyPayloadState::Pending
            }
            _ => KittyPayloadState::Unavailable,
        }
    }
}

fn spawn_transcode(key: u64, data: Arc<str>) {
    let spawned = std::thread::Builder::new()
        .name("pa-image-transcode".to_string())
        .spawn(move || {
            let outcome = jpeg_base64_to_png(&data);
            settle(key, outcome);
        });
    if spawned.is_err() {
        settle(key, Err(TranscodeError::Spawn));
    }
}

fn settle(key: u64, outcome: Result<KittyPayload, TranscodeError>) {
    // A failed row re-lays out as its textual fallback.
    let state = if let Ok(payload) = outcome {
        Prepared::Ready(Arc::new(payload))
    } else {
        FAILURES.fetch_add(1, Ordering::Relaxed);
        Prepared::Failed
    };
    lock(&PREPARED).insert(key, state);
    READY.notify_one();
}

/// Why a JPEG preview could not become kitty's PNG.
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
            Self::Decode(reason) => write!(f, "the JPEG does not decode: {reason}"),
            Self::Truncated => f.write_str("the decoded JPEG is truncated"),
            Self::Spawn => f.write_str("the transcode thread did not start"),
        }
    }
}

impl std::error::Error for TranscodeError {}

/// Decode a base64 JPEG, shrink it under [`MAX_TRANSCODED_EDGE`], and
/// re-encode it as a base64 PNG.
pub(crate) fn jpeg_base64_to_png(data: &str) -> Result<KittyPayload, TranscodeError> {
    use base64::Engine;
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|_| TranscodeError::Base64)?;
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
    let factor = width.max(height).div_ceil(MAX_TRANSCODED_EDGE).max(1);
    let (rgb, width, height) = shrink_rgb(&rgb, width, height, factor);
    let png = encode_png_rgb(&rgb, width, height);
    Ok(KittyPayload {
        base64: Arc::from(base64::engine::general_purpose::STANDARD.encode(png)),
        width_px: width,
        height_px: height,
    })
}

/// Box-filter `rgb` down by an integer `factor` (edge pixels average the
/// partial box).
pub(crate) fn shrink_rgb(rgb: &[u8], width: u32, height: u32, factor: u32) -> (Vec<u8>, u32, u32) {
    if factor <= 1 {
        return (
            rgb[..width as usize * height as usize * 3].to_vec(),
            width,
            height,
        );
    }
    let (out_width, out_height) = (width.div_ceil(factor), height.div_ceil(factor));
    let mut out = Vec::with_capacity(out_width as usize * out_height as usize * 3);
    for oy in 0..out_height {
        for ox in 0..out_width {
            let mut sum = [0u32; 3];
            let mut count = 0u32;
            for y in oy * factor..((oy + 1) * factor).min(height) {
                for x in ox * factor..((ox + 1) * factor).min(width) {
                    let at = (y as usize * width as usize + x as usize) * 3;
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += u32::from(rgb[at + channel]);
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
    let stride = width as usize * 3;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgb.chunks_exact(stride).take(height as usize) {
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
    header.extend([8, 2, 0, 0, 0]);
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
