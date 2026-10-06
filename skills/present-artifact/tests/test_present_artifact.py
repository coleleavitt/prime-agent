"""present_artifact: a display-only presentation through `artifact.present` (upstream #1062)."""

from __future__ import annotations

import asyncio
import base64
import io
import sys
import tempfile
import types
import unittest
from pathlib import Path

from PIL import Image
from present_artifact import present_artifact


class PresentArtifactTest(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.root = Path(self._dir.name)
        self.requests: list[tuple[str, dict]] = []
        saved = sys.modules.get("rlm")
        self.addCleanup(lambda: sys.modules.__setitem__("rlm", saved) if saved else sys.modules.pop("rlm", None))
        fake = types.ModuleType("rlm")

        async def host_request(request_type, payload=None):
            self.requests.append((request_type, payload))
            return {"presentationId": "p-1"}

        fake.host_request = host_request
        sys.modules["rlm"] = fake

    def _run(self, *args, **kwargs):
        return asyncio.run(present_artifact.run(*args, **kwargs))

    def test_a_small_image_rides_as_its_own_preview(self):
        png = self.root / "small.png"
        Image.new("RGB", (4, 2), "red").save(png, format="PNG")
        self.assertEqual(self._run(str(png), label="Direction A"), {"presentationId": "p-1"})
        self.assertEqual(
            self.requests,
            [
                (
                    "artifact.present",
                    {
                        "path": str(png.resolve()),
                        "label": "Direction A",
                        "preview": {
                            "original_width": 4,
                            "original_height": 2,
                            "data": base64.b64encode(png.read_bytes()).decode("ascii"),
                            "mime_type": "image/png",
                            "width": 4,
                            "height": 2,
                        },
                    },
                )
            ],
        )

    def test_a_large_image_is_bounded_to_a_jpeg_preview(self):
        png = self.root / "large.png"
        Image.linear_gradient("L").resize((3200, 1600)).convert("RGB").save(png, format="PNG")
        self._run(str(png))
        preview = self.requests[0][1]["preview"]
        self.assertEqual((preview["width"], preview["height"]), (1600, 800))
        self.assertEqual((preview["original_width"], preview["original_height"]), (3200, 1600))
        self.assertEqual(preview["mime_type"], "image/jpeg")
        self.assertLessEqual(len(preview["data"]), 350_000)
        decoded = Image.open(io.BytesIO(base64.b64decode(preview["data"])))
        self.assertEqual(decoded.size, (1600, 800))

    def test_a_generic_file_sends_no_preview(self):
        report = self.root / "report.csv"
        report.write_text("a,b\n")
        self._run(str(report))
        self.assertEqual(self.requests, [("artifact.present", {"path": str(report.resolve())})])

    def test_bad_arguments_fail_before_any_request(self):
        with self.assertRaises(FileNotFoundError):
            self._run(str(self.root / "missing.png"))
        with self.assertRaises(FileNotFoundError):
            self._run(str(self.root))
        with self.assertRaises(TypeError):
            self._run(5)
        with self.assertRaises(TypeError):
            self._run(str(self.root), label=3)
        broken = self.root / "broken.png"
        broken.write_bytes(b"\x89PNG\r\n\x1a\n" + b"\0" * 8)
        with self.assertRaises(ValueError):
            self._run(str(broken))
        self.assertEqual(self.requests, [])


if __name__ == "__main__":
    unittest.main()
