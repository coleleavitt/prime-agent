"""Present an on-disk artifact to the user, never to the model.

The kernel prepares the bounded inline preview (the host has no image codec);
the host validates the request, captures the file, and records a display-only
row the model's context never carries (`artifact.present`).
"""

from __future__ import annotations

import base64
import io
from pathlib import Path
from typing import Any

# Keep in sync with pa-core `session_engine::presented_artifact`.
_MAX_SOURCE_BYTES = 20 * 1024 * 1024
_MAX_PREVIEW_CHARS = 350_000
_MAX_PREVIEW_DIMENSION = 1600
_MAX_SOURCE_PIXELS = 36_000_000
_TRANSPARENCY_BACKGROUND = "#888888"
_JPEG_QUALITIES = (85, 75, 62, 50, 38)

_IMAGE_SIGNATURES = (
    ("image/png", b"\x89PNG\r\n\x1a\n"),
    ("image/jpeg", b"\xff\xd8\xff"),
    ("image/gif", b"GIF87a"),
    ("image/gif", b"GIF89a"),
)


def _detect_image_mime(data: bytes) -> str | None:
    for mime, prefix in _IMAGE_SIGNATURES:
        if data.startswith(prefix):
            return mime
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    return None


def _b64_chars(size: int) -> int:
    return ((size + 2) // 3) * 4


def _preview(data: bytes, mime: str) -> dict[str, Any] | None:
    """The bounded inline preview of a raster image, or None without Pillow."""
    try:
        from PIL import Image, ImageOps
    except ImportError:
        return None
    try:
        image = Image.open(io.BytesIO(data))
        width, height = image.size
        if width * height > _MAX_SOURCE_PIXELS:
            raise ValueError(
                f"image is {width}x{height}; previews need at most {_MAX_SOURCE_PIXELS // 1_000_000}MP"
            )
        if getattr(image, "is_animated", False):
            image.seek(0)
        image = ImageOps.exif_transpose(image)
        image.load()
    except ValueError:
        raise
    except Exception as error:
        raise ValueError("not a readable supported image (PNG, JPEG, GIF, WebP)") from error
    original_width, original_height = image.size
    preview = {"original_width": original_width, "original_height": original_height}
    if max(original_width, original_height) <= _MAX_PREVIEW_DIMENSION and _b64_chars(len(data)) <= _MAX_PREVIEW_CHARS:
        return {
            **preview,
            "data": base64.b64encode(data).decode("ascii"),
            "mime_type": mime,
            "width": original_width,
            "height": original_height,
        }
    transparent = image.mode in {"RGBA", "LA"} or (image.mode == "P" and "transparency" in image.info)
    if transparent:
        rgba = image.convert("RGBA")
        flat = Image.new("RGB", rgba.size, _TRANSPARENCY_BACKGROUND)
        flat.paste(rgba, mask=rgba.split()[-1])
        image = flat
    else:
        image = image.convert("RGB")
    scale = min(1.0, _MAX_PREVIEW_DIMENSION / max(original_width, original_height))
    target = (max(1, round(original_width * scale)), max(1, round(original_height * scale)))
    while True:
        resized = image.resize(target, Image.Resampling.LANCZOS)
        for quality in _JPEG_QUALITIES:
            buffer = io.BytesIO()
            resized.save(buffer, format="JPEG", quality=quality, optimize=True)
            encoded = buffer.getvalue()
            if _b64_chars(len(encoded)) <= _MAX_PREVIEW_CHARS:
                return {
                    **preview,
                    "data": base64.b64encode(encoded).decode("ascii"),
                    "mime_type": "image/jpeg",
                    "width": target[0],
                    "height": target[1],
                }
        smaller = (max(1, int(target[0] * 0.75)), max(1, int(target[1] * 0.75)))
        if smaller == target:
            raise ValueError("could not be compressed into a bounded preview")
        target = smaller


async def run(path: str, label: str | None = None) -> dict[str, Any]:
    """Show an on-disk artifact to the user without attaching it to the model's context.

    Use this when the user should SEE a result (a generated image, a chart, a
    screenshot, a report file) before it is exported, committed, or approved.
    To look at an image yourself, use `attach_image` instead.

    Args:
        path: An existing regular file (relative, absolute, or `~`-prefixed).
        label: Optional user-visible caption (at most 500 characters).

    Returns:
        The host's receipt: `artifactId`, `presentationId`, `kind` (`image` or
        `file`), `name`, `mimeType`, `byteSize`, the captured `path`, and for
        images the preview `width`/`height` and `originalWidth`/`originalHeight`.

    Raises:
        FileNotFoundError: If `path` is not an existing regular file.
        ValueError: If the file is over 20 MiB, or an image that cannot be
            previewed.
    """
    if not isinstance(path, str):
        raise TypeError(f"path must be str, got {type(path).__name__}")
    if label is not None and not isinstance(label, str):
        raise TypeError(f"label must be str or None, got {type(label).__name__}")
    filepath = Path(path).expanduser().resolve()
    if not filepath.is_file():
        raise FileNotFoundError(f"{path} is not an existing regular file")
    size = filepath.stat().st_size
    if size > _MAX_SOURCE_BYTES:
        raise ValueError(f"{path} is {size // (1024 * 1024)}MiB; artifacts must be at most 20 MiB")

    payload: dict[str, Any] = {"path": str(filepath)}
    if label is not None:
        payload["label"] = label
    with filepath.open("rb") as handle:
        head = handle.read(16)
    mime = _detect_image_mime(head)
    if mime is not None:
        try:
            preview = _preview(filepath.read_bytes(), mime)
        except ValueError as error:
            raise ValueError(f"{path}: {error}") from error
        if preview is not None:
            payload["preview"] = preview

    from rlm import host_request

    return await host_request("artifact.present", payload)
