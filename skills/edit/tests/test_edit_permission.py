"""The edit skill asks the host (`file.edit`) before it writes a file."""

from __future__ import annotations

import asyncio
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

import edit


class _Unavailable(RuntimeError):
    pass


def _fake_rlm(answer):
    """A stand-in `rlm` package whose `host_request` answers with `answer`."""
    calls: list[tuple[str, dict]] = []

    async def host_request(request_type, payload=None):
        calls.append((request_type, payload))
        return answer(request_type)

    rlm = types.ModuleType("rlm")
    rlm.host_request = host_request
    rlm.emit = lambda data: None
    repl = types.ModuleType("rlm.repl")
    repl.HostRequestUnavailable = _Unavailable
    repl.UNSERVED_HOST_REQUEST = "is not available in this session"
    rlm.repl = repl
    return {"rlm": rlm, "rlm.repl": repl}, calls


class EditPermissionTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = Path(self.dir.name) / "a.txt"
        self.path.write_text("hello world\n", encoding="utf-8")

    def tearDown(self):
        self.dir.cleanup()

    def run_edit(self, answer):
        modules, calls = _fake_rlm(answer)
        with mock.patch.dict(sys.modules, modules):
            result = asyncio.run(edit.run(str(self.path), "world", "there"))
        return result, calls

    def test_an_allowed_edit_asks_first_and_writes(self):
        result, calls = self.run_edit(lambda _type: {"allowed": True})
        payload = {"path": str(self.path.resolve())}
        # The permission check comes first; the /rewind checkpoint reports
        # bracket the write that follows it.
        self.assertEqual(
            calls,
            [
                ("file.edit", payload),
                ("file_history.before_write", payload),
                ("file_history.after_write", payload),
            ],
        )
        self.assertTrue(result.startswith("Edited "))
        self.assertEqual(self.path.read_text(encoding="utf-8"), "hello there\n")

    def test_a_refused_edit_raises_and_writes_nothing(self):
        def refuse(request_type):
            raise RuntimeError(f"{request_type} refused: Cannot edit while in plan mode")

        modules, calls = _fake_rlm(refuse)
        with mock.patch.dict(sys.modules, modules), self.assertRaises(PermissionError) as raised:
            asyncio.run(edit.run(str(self.path), "world", "there"))
        self.assertIn("refused", str(raised.exception))
        # Refused before the checkpoint: nothing was backed up or written.
        self.assertEqual([request for request, _ in calls], ["file.edit"])
        self.assertEqual(self.path.read_text(encoding="utf-8"), "hello world\n")

    def test_a_host_that_does_not_gate_edits_lets_it_through(self):
        def unserved(request_type):
            raise RuntimeError(
                f'host request type "{request_type}" is not available in this session'
            )

        self.run_edit(unserved)
        self.assertEqual(self.path.read_text(encoding="utf-8"), "hello there\n")

        def outside(_request_type):
            raise _Unavailable("repl runtime is not serving")

        self.path.write_text("hello world\n", encoding="utf-8")
        self.run_edit(outside)
        self.assertEqual(self.path.read_text(encoding="utf-8"), "hello there\n")


if __name__ == "__main__":
    unittest.main()
