//! Pasted-image downscale pipeline — the Claude Code (v2.1.316) `ust`
//! algorithm ported to Rust. Every image that enters prime-agent through a
//! paste (clipboard or file path) funnels through [`enforce`] before it
//! reaches the provider. The defaults match CC's `dA` limits object, so a
//! pasted image never hits Anthropic's many-image 3000-pixel-per-dimension
//! cap.
//!
//! Evidence: CC v2.1.316 bundle, `chunk-mvvq2nx1.js`:
//! - `aQt = 3000` (many-image side limit; the exact API error);
//! - `K = gJ = 8000` (single-image hard limit);
//! - `De = ke = 1568` (token-budget target);
//! - `dA = {maxWidth:2000, maxHeight:2000, maxBase64Size:5 MiB,
//!    targetRawSize:3.75 MiB}` (chunk-t0gq9gyw.js);
//! - `ust`: decode → `J` (scale to maxWidth/maxHeight) → `sharp.resize(...,
//!   {fit:"inside", withoutEnlargement:true})` → cascade through PNG and
//!   JPEG-quality steps until the raw buffer fits `targetRawSize`.

use image::{DynamicImage, ExtendedColorType, GenericImageView, ImageFormat, ImageReader};

/// Claude Code's default limits (`dA` in `chunk-t0gq9gyw.js:5:2843`).
pub const DEFAULT_LIMITS: Limits = Limits {
    max_width: 2000,
    max_height: 2000,
    reject_max_width: SINGLE_IMAGE_SIDE_LIMIT,
    reject_max_height: SINGLE_IMAGE_SIDE_LIMIT,
    target_raw_size: 3_932_160,
    max_base64_size: 5_242_880,
};

/// The Anthropic many-image side limit (CC `aQt` = 3000). Over this and the
/// provider rejects with `image_dimensions_too_large` for many-image
/// requests; [`DEFAULT_LIMITS`] downscales to `max_width = 2000`, well
/// below this, so a conforming paste never hits it.
pub const MANY_IMAGE_SIDE_LIMIT: u32 = 3000;

/// Anthropic's single-image hard limit (CC `K` = `gJ` = 8000). An image
/// over this cannot be sent even standalone, so it is [`enforce`]'s wall.
pub const SINGLE_IMAGE_SIDE_LIMIT: u32 = 8000;

/// Resize/encode limits for one call to [`enforce`]. Matches CC's `limits`
/// argument shape (`ust({limits})` in `chunk-mvvq2nx1.js`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Scale the image's width down to this; CC `o.maxWidth`.
    pub max_width: u32,
    /// Scale the image's height down to this; CC `o.maxHeight`.
    pub max_height: u32,
    /// Refuse the paste above this width (nothing a resize does helps the
    /// API accept it). CC's `W(o).rejectMaxWidth` — defaults to the
    /// single-image hard cap of 8000.
    pub reject_max_width: u32,
    /// Same for height.
    pub reject_max_height: u32,
    /// Try to keep the encoded raw bytes under this; CC `o.targetRawSize`.
    pub target_raw_size: usize,
    /// Absolute wire cap on the base64 payload; CC `o.maxBase64Size`.
    pub max_base64_size: usize,
}

/// A successful enforcement: the downscaled bytes, its mime type and the
/// original/display dimensions. `data` is the raw image bytes (not base64).
#[derive(Debug, Clone)]
pub struct Enforced {
    /// Encoded image bytes (not base64).
    pub data: Vec<u8>,
    /// The mime type `data` is encoded as (`image/png`, `image/jpeg`,
    /// `image/gif` or `image/webp`).
    pub mime_type: &'static str,
    /// The original image's `(width, height)` in pixels.
    pub original: (u32, u32),
    /// The resized image's `(width, height)` in pixels. May equal
    /// `original` when no resize was needed.
    pub display: (u32, u32),
}

/// Why [`enforce`] refused to downscale an image.
#[derive(Debug, Clone)]
pub enum ImageError {
    /// The byte slice did not decode as any supported image.
    Decode(String),
    /// The image's dimensions are over the hard-reject cap, so no amount of
    /// resizing would let it pass.
    Oversize {
        /// Original width in pixels.
        width: u32,
        /// Original height in pixels.
        height: u32,
        /// The configured `(reject_max_width, reject_max_height)`.
        reject: (u32, u32),
    },
    /// Even the last-resort JPEG was larger than `max_base64_size * 3 / 4`
    /// (the raw-byte equivalent of the base64 wire cap).
    StillTooLarge {
        /// The smallest raw size the cascade produced.
        raw_size: usize,
        /// The configured `max_base64_size * 3 / 4`.
        max_raw: usize,
    },
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(message) => write!(formatter, "image decode failed: {message}"),
            Self::Oversize {
                width,
                height,
                reject,
            } => write!(
                formatter,
                "image is {width}x{height} px, over the {}x{} px limit; resize it to under {}x{} \
                 and paste again",
                reject.0, reject.1, reject.0, reject.1,
            ),
            Self::StillTooLarge { raw_size, max_raw } => write!(
                formatter,
                "image is {raw_size} bytes after compression, over the {max_raw}-byte raw \
                 cap; shrink it manually and paste again"
            ),
        }
    }
}

impl std::error::Error for ImageError {}

/// Shrink `(w, h)` to fit inside `limits.max_width × limits.max_height`
/// preserving aspect ratio. Direct port of CC's `J` function in
/// `chunk-mvvq2nx1.js:5:6745`.
#[must_use]
pub fn scale_to_fit(w: u32, h: u32, limits: &Limits) -> (u32, u32) {
    let mut width = w;
    let mut height = h;
    if width > limits.max_width {
        let scaled = (u64::from(height) * u64::from(limits.max_width) + u64::from(width) / 2)
            / u64::from(width).max(1);
        height = u32::try_from(scaled).unwrap_or(u32::MAX).max(1);
        width = limits.max_width;
    }
    if height > limits.max_height {
        let scaled = (u64::from(width) * u64::from(limits.max_height) + u64::from(height) / 2)
            / u64::from(height).max(1);
        width = u32::try_from(scaled).unwrap_or(u32::MAX).max(1);
        height = limits.max_height;
    }
    (width, height)
}

/// Downscale `bytes` so the resulting image fits `limits` and is under
/// `limits.target_raw_size` raw bytes. Mirrors CC's `ust`
/// (`chunk-mvvq2nx1.js:5:8895`): decode → `scale_to_fit` → Lanczos resize
/// (sharp's `{fit:"inside", withoutEnlargement:true}`) → encode cascade
/// (source format → JPEG quality ladder → last-resort 1000-pixel JPEG).
///
/// # Errors
///
/// Returns [`ImageError::Decode`] when the bytes are not a supported image,
/// [`ImageError::Oversize`] when the dimensions exceed the reject cap
/// (nothing a resize could do would get the provider to accept it), or
/// [`ImageError::StillTooLarge`] when the cascade cannot squeeze the image
/// below the base64 wire cap.
pub fn enforce(bytes: &[u8], mime_type: &str, limits: &Limits) -> Result<Enforced, ImageError> {
    let reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| ImageError::Decode(error.to_string()))?;
    let (orig_w, orig_h) = reader
        .into_dimensions()
        .map_err(|error| ImageError::Decode(error.to_string()))?;

    if orig_w > limits.reject_max_width || orig_h > limits.reject_max_height {
        return Err(ImageError::Oversize {
            width: orig_w,
            height: orig_h,
            reject: (limits.reject_max_width, limits.reject_max_height),
        });
    }

    let (target_w, target_h) = scale_to_fit(orig_w, orig_h, limits);
    let needs_resize = target_w != orig_w || target_h != orig_h;

    let source_mime = canonical_mime(mime_type);
    if !needs_resize && bytes.len() <= limits.target_raw_size {
        return Ok(Enforced {
            data: bytes.to_vec(),
            mime_type: source_mime,
            original: (orig_w, orig_h),
            display: (target_w, target_h),
        });
    }

    let decoded =
        image::load_from_memory(bytes).map_err(|error| ImageError::Decode(error.to_string()))?;
    let resized = if needs_resize {
        decoded.resize(target_w, target_h, image::imageops::FilterType::Lanczos3)
    } else {
        decoded
    };

    let (buf, out_mime) = encode_cascade(&resized, source_mime, limits)?;
    Ok(Enforced {
        data: buf,
        mime_type: out_mime,
        original: (orig_w, orig_h),
        display: (target_w, target_h),
    })
}

/// Normalize a supplied mime type to one of the four CC-supported formats
/// (`image/png`, `image/jpeg`, `image/gif`, `image/webp`), defaulting to
/// PNG when the caller passed something unrelated. Mirrors CC's whitelist
/// `pe = new Set(["png","jpeg","gif","webp"])`.
#[must_use]
pub fn canonical_mime(mime: &str) -> &'static str {
    match mime.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" | "jpeg" | "jpg" => "image/jpeg",
        "image/gif" | "gif" => "image/gif",
        "image/webp" | "webp" => "image/webp",
        _ => "image/png",
    }
}

/// Encode `image` into the smallest raw buffer under `limits.target_raw_size`
/// following CC's ladder:
/// 1. Encode as `source_mime` at default fidelity.
/// 2. Transcode to PNG if `source_mime` was not already PNG.
/// 3. JPEG at quality 80, 60, 40, 20.
/// 4. Resize to max 1000 px wide and JPEG at quality 20.
fn encode_cascade(
    img: &DynamicImage,
    source_mime: &'static str,
    limits: &Limits,
) -> Result<(Vec<u8>, &'static str), ImageError> {
    let first = encode_as(img, source_mime)?;
    if first.len() <= limits.target_raw_size {
        return Ok((first, source_mime));
    }
    let mut best: (Vec<u8>, &'static str) = (first, source_mime);

    if source_mime != "image/png" {
        let png = encode_as(img, "image/png")?;
        if png.len() <= limits.target_raw_size {
            return Ok((png, "image/png"));
        }
        if png.len() < best.0.len() {
            best = (png, "image/png");
        }
    }

    for &quality in &[80u8, 60, 40, 20] {
        let jpeg = encode_jpeg(img, quality)?;
        if jpeg.len() <= limits.target_raw_size {
            return Ok((jpeg, "image/jpeg"));
        }
        if jpeg.len() < best.0.len() {
            best = (jpeg, "image/jpeg");
        }
    }

    let (w, h) = img.dimensions();
    let small_w = w.min(1000);
    let small_h = if w == 0 {
        h
    } else {
        u32::try_from((u64::from(h) * u64::from(small_w)) / u64::from(w).max(1))
            .unwrap_or(u32::MAX)
            .max(1)
    };
    let last = img.resize(small_w, small_h, image::imageops::FilterType::Lanczos3);
    let final_jpeg = encode_jpeg(&last, 20)?;
    let max_raw = limits.max_base64_size.saturating_mul(3) / 4;
    if final_jpeg.len() <= max_raw {
        return Ok((final_jpeg, "image/jpeg"));
    }
    if final_jpeg.len() < best.0.len() {
        best = (final_jpeg, "image/jpeg");
    }
    Err(ImageError::StillTooLarge {
        raw_size: best.0.len(),
        max_raw,
    })
}

fn encode_as(img: &DynamicImage, mime: &str) -> Result<Vec<u8>, ImageError> {
    let format = match mime {
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::WebP,
        _ => ImageFormat::Png,
    };
    // JPEG and WebP cannot carry an alpha channel; RGBA sources would
    // otherwise round-trip as transparent black. Flatten first.
    let payload = match format {
        ImageFormat::Jpeg => DynamicImage::ImageRgb8(img.to_rgb8()),
        _ => img.clone(),
    };
    let mut buf = Vec::new();
    payload
        .write_to(&mut std::io::Cursor::new(&mut buf), format)
        .map_err(|error| ImageError::Decode(error.to_string()))?;
    Ok(buf)
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, ImageError> {
    use image::codecs::jpeg::JpegEncoder;
    let rgb = img.to_rgb8();
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, quality)
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|error| ImageError::Decode(error.to_string()))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use image::{ImageBuffer, Rgb};

    use super::*;

    /// Build a synthetic PNG of `(w, h)` with a cheap gradient; its
    /// dimensions pass whatever check the test needs, and its pixels are
    /// non-constant so palette-9 PNG cannot cheat the size cascade.
    fn make_png(width: u32, height: u32) -> Vec<u8> {
        let img = ImageBuffer::from_fn(width, height, |x, y| {
            Rgb([((x ^ y) & 0xff) as u8, (x & 0xff) as u8, (y & 0xff) as u8])
        });
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn scale_to_fit_matches_cc_j_function() {
        let limits = DEFAULT_LIMITS;
        assert_eq!(scale_to_fit(1000, 500, &limits), (1000, 500));
        assert_eq!(scale_to_fit(2000, 1000, &limits), (2000, 1000));
        assert_eq!(scale_to_fit(4000, 2000, &limits), (2000, 1000));
        assert_eq!(scale_to_fit(3088, 1813, &limits), (2000, 1174));
        assert_eq!(scale_to_fit(500, 4000, &limits), (250, 2000));
    }

    #[test]
    fn under_cap_passthrough_keeps_bytes_exact() {
        let bytes = make_png(400, 300);
        let out = enforce(&bytes, "image/png", &DEFAULT_LIMITS).expect("enforce");
        assert_eq!(out.original, (400, 300));
        assert_eq!(out.display, (400, 300));
        assert_eq!(out.mime_type, "image/png");
        assert_eq!(out.data, bytes, "bytes should be returned unchanged");
    }

    #[test]
    fn oversize_png_downscales_under_max_side() {
        let bytes = make_png(3088, 1813);
        let out = enforce(&bytes, "image/png", &DEFAULT_LIMITS).expect("enforce");
        assert_eq!(out.original, (3088, 1813));
        assert!(
            out.display.0 <= DEFAULT_LIMITS.max_width && out.display.1 <= DEFAULT_LIMITS.max_height,
            "display {:?} must fit {}x{}",
            out.display,
            DEFAULT_LIMITS.max_width,
            DEFAULT_LIMITS.max_height,
        );
        // `image::load_from_memory` would err on the result if the encoder
        // produced something broken.
        let reread = image::load_from_memory(&out.data).expect("round-trip decode");
        assert_eq!(reread.dimensions(), out.display);
    }

    #[test]
    fn hard_reject_above_single_image_limit() {
        // Build a tiny PNG then lie about its dimensions via a bigger
        // synthesized buffer — this would be wasteful. Easier: lower the
        // reject cap for the test so a 400x300 PNG trips it.
        let bytes = make_png(400, 300);
        let strict = Limits {
            reject_max_width: 200,
            reject_max_height: 200,
            ..DEFAULT_LIMITS
        };
        let result = enforce(&bytes, "image/png", &strict);
        match result {
            Err(ImageError::Oversize {
                width,
                height,
                reject,
            }) => {
                assert_eq!((width, height), (400, 300));
                assert_eq!(reject, (200, 200));
            }
            other => panic!("expected Oversize, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_bytes_are_a_decode_error() {
        let result = enforce(b"not an image", "image/png", &DEFAULT_LIMITS);
        assert!(matches!(result, Err(ImageError::Decode(_))));
    }

    #[test]
    fn jpeg_input_round_trips_as_jpeg() {
        let rgb: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_fn(500, 400, |x, y| Rgb([x as u8, y as u8, (x ^ y) as u8]));
        let mut jpeg = Vec::new();
        DynamicImage::ImageRgb8(rgb)
            .write_to(&mut std::io::Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .unwrap();
        let out = enforce(&jpeg, "image/jpeg", &DEFAULT_LIMITS).expect("enforce");
        assert_eq!(out.mime_type, "image/jpeg");
        assert_eq!(out.original, (500, 400));
    }

    #[test]
    fn mime_canonicalization_covers_aliases() {
        assert_eq!(canonical_mime("image/jpg"), "image/jpeg");
        assert_eq!(canonical_mime("image/JPEG"), "image/jpeg");
        assert_eq!(canonical_mime("image/gif"), "image/gif");
        assert_eq!(canonical_mime("image/webp"), "image/webp");
        assert_eq!(canonical_mime("image/png"), "image/png");
        assert_eq!(canonical_mime("text/plain"), "image/png");
    }
}
