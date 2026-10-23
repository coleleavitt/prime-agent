"""The edit skill's write reports for /rewind file checkpoints.

Run: ``PYTHONPATH=src python -m unittest discover -s tests`` from
``skills/edit``.
"""

from __future__ import annotations

import asyncio
import sys
import tempfile
import types
import unittest
from pathlib import Path

import edit


class _FakeRlm(types.ModuleType):
    def __init__(self, fail: bool) -> None:
        super().__init__("rlm")
        self.calls: list[tuple[str, str, str]] = []
        self.fail = fail

    async def host_request(self, request_type: str, payload: dict) -> dict:
        path = payload["path"]
        self.calls.append((request_type, path, Path(path).read_text(encoding="utf-8")))
        if self.fail:
            raise RuntimeError("file_history.before_write is not available in this session")
        return {"recorded": True}

    def emit(self, data: dict) -> None:
        pass


class EditReportsTest(unittest.TestCase):
    def _run(self, fail: bool) -> tuple[_FakeRlm, Path]:
        fake = _FakeRlm(fail)
        previous = sys.modules.get("rlm")
        sys.modules["rlm"] = fake
        try:
            with tempfile.TemporaryDirectory() as tmp:
                target = Path(tmp) / "file.txt"
                target.write_text("alpha\nbeta\n", encoding="utf-8")
                asyncio.run(edit.run(str(target), "beta", "gamma"))
                self.assertEqual(target.read_text(encoding="utf-8"), "alpha\ngamma\n")
                return fake, target.resolve()
        finally:
            if previous is None:
                sys.modules.pop("rlm", None)
            else:
                sys.modules["rlm"] = previous

    def test_the_write_is_bracketed_by_its_reports(self) -> None:
        fake, target = self._run(fail=False)
        self.assertEqual(
            fake.calls,
            [
                ("file_history.before_write", str(target), "alpha\nbeta\n"),
                ("file_history.after_write", str(target), "alpha\ngamma\n"),
            ],
        )

    def test_a_failed_report_never_fails_the_edit(self) -> None:
        fake, _ = self._run(fail=True)
        self.assertEqual(len(fake.calls), 2)


if __name__ == "__main__":
    unittest.main()
