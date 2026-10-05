"""attach_image on a text-only session model delegates to the image model (vision.read)."""

from __future__ import annotations

import asyncio
import base64
import sys
import tempfile
import types
import unittest
from pathlib import Path

from attach_image import attach_image
from PIL import Image

UNAVAILABLE = 'host request type "vision.read" is not available in this session'


class _FakeHost:
    """The `rlm` module the skill imports: records every host request and emit."""

    def __init__(self, model_input: list[str], vision_read):
        self.model_input = model_input
        self.vision_read = vision_read
        self.requests: list[tuple[str, dict | None]] = []
        self.emitted: list[dict] = []

    def module(self) -> types.ModuleType:
        module = types.ModuleType("rlm")

        async def host_request(request_type, payload=None):
            self.requests.append((request_type, payload))
            if request_type == "model.info":
                return {"id": "mock-1", "provider": "battery", "input": self.model_input}
            if request_type == "vision.read":
                return self.vision_read(payload)
            raise AssertionError(f"unexpected host request {request_type}")

        module.host_request = host_request
        module.emit = self.emitted.append
        return module


class VisionDelegationTest(unittest.TestCase):
    def setUp(self):
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.png = Path(self._dir.name) / "square.png"
        Image.new("RGB", (4, 4), "red").save(self.png, format="PNG")
        self.png_b64 = base64.b64encode(self.png.read_bytes()).decode("ascii")
        self._saved_rlm = sys.modules.get("rlm")
        self.addCleanup(self._restore_rlm)

    def _restore_rlm(self):
        if self._saved_rlm is None:
            sys.modules.pop("rlm", None)
        else:
            sys.modules["rlm"] = self._saved_rlm

    def _run(self, host: _FakeHost, *paths: str) -> str:
        sys.modules["rlm"] = host.module()
        return asyncio.run(attach_image.run(*paths))

    def test_a_text_only_session_reads_the_image_with_the_image_model(self):
        host = _FakeHost(["text"], lambda _payload: {"text": "a red square", "model": "battery/mock-vision"})
        result = self._run(host, str(self.png))
        self.assertEqual(
            result,
            "a red square\n\n(Read by battery/mock-vision; the session model cannot see images.)",
        )
        self.assertEqual(
            host.requests,
            [
                ("model.info", None),
                (
                    "vision.read",
                    {
                        "images": [{"data": self.png_b64, "mime_type": "image/png"}],
                        "question": attach_image._VISION_READ_QUESTION,
                    },
                ),
            ],
        )
        self.assertEqual(host.emitted, [], "no attachment enters the text-only session")

    def test_a_host_without_vision_read_keeps_the_vision_error(self):
        def unavailable(_payload):
            raise RuntimeError(UNAVAILABLE)

        host = _FakeHost(["text"], unavailable)
        with self.assertRaises(RuntimeError) as raised:
            self._run(host, str(self.png))
        self.assertEqual(
            str(raised.exception),
            "mock-1 does not support vision. "
            "Tell the user to switch to a vision-capable model to load images into context.",
        )

    def test_the_hosts_refusal_is_the_error(self):
        refusal = 'imageModel "nope" could not be resolved to an available, image-capable, authenticated model.'

        def refused(_payload):
            raise RuntimeError(refusal)

        host = _FakeHost(["text"], refused)
        with self.assertRaises(RuntimeError) as raised:
            self._run(host, str(self.png))
        self.assertEqual(str(raised.exception), refusal)

    def test_an_empty_reading_keeps_the_vision_error(self):
        host = _FakeHost(["text"], lambda _payload: {"text": "  ", "model": "battery/mock-vision"})
        with self.assertRaises(RuntimeError) as raised:
            self._run(host, str(self.png))
        self.assertIn("does not support vision", str(raised.exception))

    def test_a_bad_path_fails_before_any_read(self):
        host = _FakeHost(["text"], lambda _payload: {"text": "unused", "model": "battery/mock-vision"})
        missing = str(Path(self._dir.name) / "missing.png")
        with self.assertRaises(FileNotFoundError):
            self._run(host, missing)
        self.assertEqual(host.requests, [("model.info", None)])

    def test_a_vision_session_still_attaches_the_image(self):
        host = _FakeHost(["text", "image"], lambda _payload: {"text": "unused", "model": "unused"})
        result = self._run(host, str(self.png))
        self.assertEqual(result, f"Loaded 1 image(s) into context: {self.png}")
        self.assertEqual(host.requests, [("model.info", None)])
        self.assertEqual(
            host.emitted,
            [
                {
                    attach_image._ATTACHMENT_DISPLAY_MIME: {
                        "mime_type": "image/png",
                        "data": self.png_b64,
                        "path": str(self.png),
                    },
                    "text/plain": f"Loaded image into context: {self.png}",
                }
            ],
        )


if __name__ == "__main__":
    unittest.main()
