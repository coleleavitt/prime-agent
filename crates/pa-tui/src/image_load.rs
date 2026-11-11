//! Image loading from disk with MIME-type detection for the supported
//! attachment formats (PNG, JPEG, GIF, WebP - the attach-image skill's
//! formats). The contract is the TS image path: sniff a bounded prefix
//! of the file's bytes (never trust the extension), keep only the
//! supported image types, and read the payload as base64.

use std::path::Path;

use base64::Engine;

use crate::image_limits;

/// The supported attachment mime types (TS `IMAGE_MIME_TYPES`).
pub const SUPPORTED_IMAGE_MIME_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/webp", "image/gif"];

const SNIFF_BYTES: usize = 4100;

/// A loaded image attachment: base64 payload plus its detected mime type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedImage {
    pub data: String,
    pub mime_type: String,
}

/// Detect the supported image mime type from leading bytes (the same
/// magic signatures the `file-type` package matches for these formats).
#[must_use]
pub fn detect_supported_image_mime_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Detect the supported image mime type of a file, reading only the
/// sniff prefix. `Ok(None)` for a non-image or empty file.
///
/// # Errors
///
/// Returns `Err` when the file cannot be opened or its sniff prefix read.
pub fn detect_supported_image_mime_from_path(path: &Path) -> std::io::Result<Option<&'static str>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; SNIFF_BYTES];
    let read = file.read(&mut buffer)?;
    buffer.truncate(read);
    if buffer.is_empty() {
        return Ok(None);
    }
    Ok(detect_supported_image_mime_from_bytes(&buffer))
}

/// Load an image file as an attachment payload. `Ok(None)` when the file
/// is empty or not a supported image (the caller treats those as plain
/// text instead). The bytes are passed through
/// [`image_limits::enforce`] so a pasted or `--image`-flagged file is
/// resized under the Anthropic many-image 3000-pixel-per-side cap before
/// it ever reaches the provider; a true-oversize image (over 8000 px on
/// either side, past the single-image hard cap) is surfaced as an
/// `InvalidData` IO error so the caller can show the user *why* the
/// paste was refused.
///
/// # Errors
///
/// Returns `Err` when the file cannot be opened, its prefix read, its
/// full bytes re-read, or when the image is over the hard-reject cap.
pub fn load_image_from_path(path: &Path) -> std::io::Result<Option<LoadedImage>> {
    let Some(mime_type) = detect_supported_image_mime_from_path(path)? else {
        return Ok(None);
    };
    let bytes = std::fs::read(path)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    match image_limits::enforce(&bytes, mime_type, &image_limits::DEFAULT_LIMITS) {
        Ok(enforced) => Ok(Some(LoadedImage {
            data: base64::engine::general_purpose::STANDARD.encode(&enforced.data),
            mime_type: enforced.mime_type.to_string(),
        })),
        Err(image_limits::ImageError::Decode(_)) => Ok(None),
        Err(error) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The image file plus its dir's guard: the caller holds the guard for
    /// the assertions' lifetime and the dir goes with the test.
    fn temp_image(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn sniffs_each_supported_format_from_magic_bytes() {
        assert_eq!(
            detect_supported_image_mime_from_bytes(&[
                0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a
            ]),
            Some("image/png")
        );
        assert_eq!(
            detect_supported_image_mime_from_bytes(&[0xff, 0xd8, 0xff, 0xe0]),
            Some("image/jpeg")
        );
        assert_eq!(
            detect_supported_image_mime_from_bytes(b"GIF89a\x01\x02"),
            Some("image/gif")
        );
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBPVP8 ");
        assert_eq!(
            detect_supported_image_mime_from_bytes(&webp),
            Some("image/webp")
        );
        assert_eq!(detect_supported_image_mime_from_bytes(b"hello"), None);
        assert_eq!(detect_supported_image_mime_from_bytes(b""), None);
    }

    #[test]
    fn sniff_ignores_extensions_and_reads_only_the_prefix() {
        let (_png_dir, png) = temp_image("notes.png", b"this is not an image at all");
        assert_eq!(
            detect_supported_image_mime_from_path(&png).unwrap(),
            None,
            "content, not the name, decides"
        );
        let (_jpeg_dir, jpeg) = temp_image("photo.txt", &[0xff, 0xd8, 0xff, 0xe0, 0, 0]);
        assert_eq!(
            detect_supported_image_mime_from_path(&jpeg).unwrap(),
            Some("image/jpeg")
        );
    }

    #[test]
    fn loads_supported_images_as_base64_attachments() {
        // A real tiny PNG round-trips through `image_limits::enforce` (which
        // decodes before accepting), so the base64 payload matches the raw
        // bytes under the target-raw-size passthrough.
        let png_bytes = {
            use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};
            let img: ImageBuffer<Rgb<u8>, Vec<u8>> =
                ImageBuffer::from_fn(2, 2, |x, y| Rgb([x as u8, y as u8, 0]));
            let mut buf = Vec::new();
            DynamicImage::ImageRgb8(img)
                .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
                .unwrap();
            buf
        };
        let (_png_dir, png) = temp_image("a.png", &png_bytes);
        let loaded = load_image_from_path(&png).unwrap().expect("image");
        assert_eq!(loaded.mime_type, "image/png");
        assert_eq!(
            loaded.data,
            base64::engine::general_purpose::STANDARD.encode(&png_bytes),
            "under-cap PNG passes through unchanged (CC `ust` passthrough branch)"
        );
        // Magic-only bytes without a decodable payload are not an image.
        let (_gif_dir, gif) = temp_image("a.gif", b"GIF89a\x02\x00\x01\x00");
        assert_eq!(
            load_image_from_path(&gif).unwrap(),
            None,
            "a GIF header without a frame no longer slips through the decode gate"
        );
        let (_text_dir, text) = temp_image("a.txt", b"just text");
        assert_eq!(load_image_from_path(&text).unwrap(), None);
        let (_empty_dir, empty) = temp_image("a.png", b"");
        assert_eq!(load_image_from_path(&empty).unwrap(), None);
    }

    #[test]
    fn missing_file_is_an_error_not_none() {
        assert!(detect_supported_image_mime_from_path(Path::new("/nonexistent/x.png")).is_err());
    }
}
