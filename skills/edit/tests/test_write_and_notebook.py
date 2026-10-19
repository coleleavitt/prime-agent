"""edit.write, edit.notebook_read / notebook_edit, and the read-state rule.

Run: ``PYTHONPATH=src python -m unittest discover -s tests`` from ``skills/edit``.
"""

from __future__ import annotations

import asyncio
import json
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

import edit

_NOTEBOOK = {
    "cells": [
        {"cell_type": "markdown", "id": "intro", "metadata": {}, "source": ["# Title\n", "text"]},
        {
            "cell_type": "code",
            "id": "calc",
            "metadata": {},
            "execution_count": 3,
            "source": "1 + 1",
            "outputs": [{"output_type": "execute_result", "data": {"text/plain": "2"}, "metadata": {}}],
        },
    ],
    "metadata": {"language_info": {"name": "python"}},
    "nbformat": 4,
    "nbformat_minor": 5,
}


def _fake_rlm(refusal: str | None = None):
    """A stand-in `rlm` recording host requests, emits and read-state calls."""
    calls: list[tuple] = []

    async def host_request(request_type, payload=None):
        calls.append((request_type, payload["path"]))
        return {}

    rlm = types.ModuleType("rlm")
    rlm.host_request = host_request
    rlm.emit = lambda data: calls.append(("emit", data))
    repl = types.ModuleType("rlm.repl")
    repl.HostRequestUnavailable = type("Unavailable", (RuntimeError,), {})
    repl.UNSERVED_HOST_REQUEST = "is not available in this session"
    file_access = types.ModuleType("rlm.file_access")

    def edit_refusal(path):
        calls.append(("edit_refusal", path))
        return refusal

    file_access.edit_refusal = edit_refusal
    file_access.note_written = lambda path: calls.append(("note_written", path))
    rlm.repl = repl
    rlm.file_access = file_access
    return {"rlm": rlm, "rlm.repl": repl, "rlm.file_access": file_access}, calls


class WriteTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)

    def test_a_write_creates_through_permission_history_and_diff(self):
        target = Path(self.dir.name) / "sub" / "new.txt"
        modules, calls = _fake_rlm()
        with mock.patch.dict(sys.modules, modules):
            result = asyncio.run(edit.write(str(target), "hello\n"))
        self.assertEqual(target.read_text(encoding="utf-8"), "hello\n")
        resolved = str(target.resolve())
        self.assertEqual(result, f"File created successfully at: {resolved}")
        kinds = [call[0] for call in calls]
        # A new file has no read state to check.
        self.assertEqual(
            kinds, ["file.edit", "file_history.before_write", "file_history.after_write", "note_written", "emit"]
        )
        diff = calls[-1][1]["application/vnd.prime-agent.diff+json"]
        self.assertEqual((diff["old_str"], diff["new_str"]), ("", "hello\n"))

    def test_an_overwrite_keeps_the_bom_and_reports_an_update(self):
        target = Path(self.dir.name) / "a.txt"
        target.write_bytes(b"\xef\xbb\xbfold\n")
        modules, calls = _fake_rlm()
        with mock.patch.dict(sys.modules, modules):
            result = asyncio.run(edit.write(str(target), "new\n"))
        self.assertEqual(target.read_bytes(), b"\xef\xbb\xbfnew\n")
        self.assertIn("has been updated successfully", result)
        self.assertEqual(calls[0][0], "edit_refusal")

    def test_a_stale_file_is_refused_before_permission_is_asked(self):
        target = Path(self.dir.name) / "a.txt"
        target.write_text("old\n", encoding="utf-8")
        modules, calls = _fake_rlm(refusal="File has been modified since read")
        with mock.patch.dict(sys.modules, modules), self.assertRaises(ValueError):
            asyncio.run(edit.write(str(target), "new\n"))
        with mock.patch.dict(sys.modules, modules), self.assertRaises(ValueError):
            asyncio.run(edit.run(str(target), "old", "new"))
        self.assertEqual([call[0] for call in calls], ["edit_refusal", "edit_refusal"])
        self.assertEqual(target.read_text(encoding="utf-8"), "old\n")


class NotebookTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.path = Path(self.dir.name) / "n.ipynb"
        self.path.write_text(json.dumps(_NOTEBOOK, indent=1) + "\n", encoding="utf-8")

    def run_async(self, coroutine):
        with mock.patch.dict(sys.modules, {"rlm": None}):
            return asyncio.run(coroutine)

    def notebook(self):
        return json.loads(self.path.read_text(encoding="utf-8"))

    def test_read_renders_cells_with_ids_and_outputs(self):
        text = self.run_async(edit.notebook_read(str(self.path)))
        self.assertEqual(
            text,
            '<cell id="intro"><cell_type>markdown</cell_type># Title\ntext</cell id="intro">\n'
            '<cell id="calc">1 + 1</cell id="calc">\n2',
        )
        self.assertEqual(
            self.run_async(edit.notebook_read(str(self.path), cell_id="calc")),
            '<cell id="calc">1 + 1</cell id="calc">\n2',
        )

    def test_replace_clears_outputs_and_keeps_the_layout(self):
        result = self.run_async(edit.notebook_edit(str(self.path), "2 + 2", "calc"))
        self.assertTrue(result.startswith("Updated cell calc"))
        cell = self.notebook()["cells"][1]
        self.assertEqual((cell["source"], cell["outputs"], cell["execution_count"]), ("2 + 2", [], None))
        self.assertTrue(self.path.read_text(encoding="utf-8").endswith("}\n"))

    def test_insert_needs_a_type_and_lands_after_the_named_cell(self):
        with self.assertRaises(ValueError) as raised:
            self.run_async(edit.notebook_edit(str(self.path), "x", "intro", edit_mode="insert"))
        self.assertEqual(str(raised.exception), "Cell type is required when using edit_mode=insert.")
        self.run_async(edit.notebook_edit(str(self.path), "x = 1", "intro", cell_type="code", edit_mode="insert"))
        cells = self.notebook()["cells"]
        self.assertEqual([cell["source"] for cell in cells], [["# Title\n", "text"], "x = 1", "1 + 1"])
        self.assertEqual(len(cells[1]["id"]), 8)
        self.run_async(edit.notebook_edit(str(self.path), "# Top", cell_type="markdown", edit_mode="insert"))
        self.assertEqual(self.notebook()["cells"][0]["source"], "# Top")

    def test_delete_and_index_ids(self):
        self.run_async(edit.notebook_edit(str(self.path), "", "cell-0", edit_mode="delete"))
        self.assertEqual([cell["id"] for cell in self.notebook()["cells"]], ["calc"])
        with self.assertRaises(ValueError) as raised:
            self.run_async(edit.notebook_edit(str(self.path), "", "nope"))
        self.assertEqual(str(raised.exception), 'Cell with ID "nope" not found in notebook.')
        with self.assertRaises(ValueError):
            self.run_async(edit.notebook_edit(str(self.path), "x"))

    def test_only_notebooks(self):
        other = Path(self.dir.name) / "a.json"
        other.write_text("{}", encoding="utf-8")
        with self.assertRaises(ValueError) as raised:
            self.run_async(edit.notebook_edit(str(other), "x", "c"))
        self.assertIn("must be a Jupyter notebook", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
