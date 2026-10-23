---
name: attach-image
description: Load an on-disk image (PNG, JPEG, GIF, WebP) or PDF pages into the model's context as a viewable attachment so the model can directly SEE it — for screenshots, diagrams, charts, photos, or scanned pages. Use this when you need to perceive an image's visual contents. On a text-only model, the configured vision-capable `imageModel` (settings.json) reads the images and returns a text description instead; errors clearly when no such image model is configured.
---

# Attach Image

Load on-disk images into the model's context as multimodal attachments. The
image is sent to the model the same way a pasted image is, so the model can
actually look at it.

## When to use this

- The user points at an image file and wants you to look at it.
- You need to read text, a chart, a diagram, or a layout from an image.
- A screenshot needs visual interpretation.

## When NOT to use this

For *programmatic* work on an image — measuring pixels, cropping, resizing,
computing a hash, comparing files byte-by-byte — open it in the kernel with a
library instead:

```python
from PIL import Image
img = Image.open("diagram.png")
print(img.size)
```

That path does not put the image in the model's context; it only lets you
compute over it. Use `attach_image` when you need to *see* the image.

## Usage

Call the prepared `attach_image` import directly in the Python kernel:

```python
print(await attach_image("diagram.png"))
print(await attach_image("a.png", "b.jpg"))
```

The skill automatically resizes and compresses large images before loading them
into context. Animated images that need compression are flattened to their first
frame. Transparent images that need compression are composited onto a neutral
gray background. Extremely large images are rejected by pixel count before full
processing. The original file is left untouched.

Supported formats: PNG, JPEG, GIF, WebP, and PDF. The skill errors if a file is
not a supported image or PDF.

## PDFs

A PDF's pages are rendered as images (poppler's `pdftoppm`, 100 dpi JPEG) and
attached like any image:

```python
print(await attach_image("paper.pdf"))               # up to 10 pages: every page
print(await attach_image("paper.pdf", pages="3-7"))  # a page or range, at most 20 per call
print(await attach_image.pdf_text("paper.pdf", pages="1-20"))  # the text layer only
```

A PDF of more than 10 pages needs `pages`. `pdf_text` returns the text layer
(`pdftotext -layout`), which is cheaper when figures and layout do not matter;
a scanned PDF has none, so attach its pages instead. Both need poppler-utils
(`pdftoppm`, `pdfinfo`, `pdftotext`); without them the skill names the install
command.

When the current model cannot see images, the images are read by the image
model configured as `imageModel` in settings.json instead: one bounded child on
that model describes them, and the call returns its text reading followed by
`(Read by <model>; the session model cannot see images.)`. No image enters the
context on that path. Without a usable image model the skill errors and names
the fix.
