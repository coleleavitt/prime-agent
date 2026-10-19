"""attach_image on PDFs: pages rendered as images (Claude Code's Read for PDFs),
and pdf_text's text layer. Needs poppler-utils; skipped without it."""

from __future__ import annotations

import asyncio
import shutil
import sys
import tempfile
import types
import unittest
from pathlib import Path

from attach_image import attach_image


def _pdf(pages: list[str]) -> bytes:
    """A minimal valid PDF: one page per string, the string as Helvetica text."""
    count = len(pages)
    page_ids = [3 + 2 * index for index in range(count)]
    font_id = 3 + 2 * count
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [" + b" ".join(f"{pid} 0 R".encode() for pid in page_ids) + b"] /Count "
        + str(count).encode()
        + b" >>",
    ]
    for index, text in enumerate(pages):
        stream = f"BT /F1 24 Tf 72 700 Td ({text}) Tj ET".encode()
        objects.append(
            f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents {page_ids[index] + 1} 0 R "
            f"/Resources << /Font << /F1 {font_id} 0 R >> >> >>".encode()
        )
        objects.append(b"<< /Length " + str(len(stream)).encode() + b" >>\nstream\n" + stream + b"\nendstream")
    objects.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for number, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{number} 0 obj\n".encode() + body + b"\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode()
    for offset in offsets:
        out += f"{offset:010d} 00000 n \n".encode()
    out += f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return bytes(out)


@unittest.skipUnless(
    all(shutil.which(tool) for tool in ("pdftoppm", "pdfinfo", "pdftotext")), "poppler-utils not installed"
)
class PdfTest(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.short = Path(self._dir.name) / "short.pdf"
        self.short.write_bytes(_pdf(["Alpha page", "Beta page"]))
        self.long = Path(self._dir.name) / "long.pdf"
        self.long.write_bytes(_pdf([f"Page {n}" for n in range(1, 13)]))
        self.emitted: list[dict] = []
        saved = sys.modules.get("rlm")
        self.addCleanup(lambda: sys.modules.__setitem__("rlm", saved) if saved else sys.modules.pop("rlm", None))
        module = types.ModuleType("rlm")

        async def host_request(request_type, payload=None):
            assert request_type == "model.info", request_type
            return {"id": "mock-1", "input": ["text", "image"]}

        module.host_request = host_request
        module.emit = self.emitted.append
        sys.modules["rlm"] = module

    def attach(self, *paths, **kwargs) -> str:
        return asyncio.run(attach_image.run(*paths, **kwargs))

    def test_a_short_pdf_attaches_every_page(self):
        result = self.attach(str(self.short))
        self.assertEqual(result, f"Loaded 2 image(s) into context: {self.short}")
        labels = [event[attach_image._ATTACHMENT_DISPLAY_MIME]["path"] for event in self.emitted]
        self.assertEqual(labels, [f"{self.short} (page 1)", f"{self.short} (page 2)"])
        self.assertTrue(all(event[attach_image._ATTACHMENT_DISPLAY_MIME]["mime_type"] == "image/jpeg" for event in self.emitted))

    def test_a_long_pdf_needs_pages(self):
        with self.assertRaises(ValueError) as raised:
            self.attach(str(self.long))
        self.assertIn("This PDF has 12 pages, which is too many to read at once", str(raised.exception))
        self.assertEqual(self.emitted, [])
        self.attach(str(self.long), pages="11-12")
        labels = [event[attach_image._ATTACHMENT_DISPLAY_MIME]["path"] for event in self.emitted]
        self.assertEqual(labels, [f"{self.long} (page 11)", f"{self.long} (page 12)"])

    def test_page_bounds(self):
        with self.assertRaises(ValueError) as raised:
            self.attach(str(self.long), pages="12-13")
        self.assertIn("is outside the document (PDF has 12 pages)", str(raised.exception))
        with self.assertRaises(ValueError):
            self.attach(str(self.long), pages="1-21")
        with self.assertRaises(ValueError):
            self.attach(str(self.long), pages="three")
        self.assertEqual(self.emitted, [])

    def test_pages_is_for_pdfs(self):
        from PIL import Image

        png = Path(self._dir.name) / "a.png"
        Image.new("RGB", (4, 4), "red").save(png, format="PNG")
        with self.assertRaises(ValueError) as raised:
            self.attach(str(png), pages="1")
        self.assertEqual(str(raised.exception), "pages applies to PDF files only")

    def test_pdf_text_reads_the_text_layer(self):
        text = asyncio.run(attach_image.pdf_text(str(self.long), pages="3-4"))
        self.assertIn("Page 3", text)
        self.assertIn("Page 4", text)
        self.assertNotIn("Page 5", text)
        self.assertIn("Beta page", asyncio.run(attach_image.pdf_text(str(self.short))))
        with self.assertRaises(ValueError):
            asyncio.run(attach_image.pdf_text(__file__))


if __name__ == "__main__":
    unittest.main()
