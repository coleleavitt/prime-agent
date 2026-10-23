"""The edit skill keeps a file's bytes apart from the replaced text, and
follows Claude Code's Edit rules (replace_all, create on an empty old_str,
quote normalisation, line deletion, refusals).

Run: ``PYTHONPATH=src python -m unittest discover -s tests`` from ``skills/edit``.
"""

from __future__ import annotations

import asyncio
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import edit


def _run(*args, **kwargs):
    # No `rlm` here: every host step is skipped, as outside a kernel.
    with mock.patch.dict(sys.modules, {"rlm": None}):
        return asyncio.run(edit.run(*args, **kwargs))


class EditFidelityTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.path = Path(self.dir.name) / "f.txt"

    def write(self, data: bytes) -> None:
        self.path.write_bytes(data)

    def test_a_crlf_file_stays_crlf(self):
        # The regression: the edit read and wrote through newline
        # translation, so every CRLF in the file became LF.
        self.write(b"a\r\nb\r\nc\r\n")
        _run(str(self.path), "a", "A")
        self.assertEqual(self.path.read_bytes(), b"A\r\nb\r\nc\r\n")

    def test_lf_text_quoted_from_a_crlf_file_matches_and_writes_crlf(self):
        self.write(b"one\r\ntwo\r\nthree\r\n")
        _run(str(self.path), "one\ntwo\n", "uno\ndos\n")
        self.assertEqual(self.path.read_bytes(), b"uno\r\ndos\r\nthree\r\n")

    def test_a_mixed_file_changes_only_the_replaced_text(self):
        self.write(b"x\r\ny\nz\r\n")
        _run(str(self.path), "y", "Y")
        self.assertEqual(self.path.read_bytes(), b"x\r\nY\nz\r\n")

    def test_a_utf8_bom_is_kept(self):
        self.write(b"\xef\xbb\xbfhello world\n")
        _run(str(self.path), "world", "there")
        self.assertEqual(self.path.read_bytes(), b"\xef\xbb\xbfhello there\n")

    def test_a_utf16le_file_is_written_back_as_utf16le(self):
        self.write(b"\xff\xfe" + "café au lait\r\n".encode("utf-16-le"))
        _run(str(self.path), "lait", "the")
        self.assertEqual(self.path.read_bytes(), b"\xff\xfe" + "café au the\r\n".encode("utf-16-le"))

    def test_a_file_that_is_not_utf8_is_refused_untouched(self):
        self.write(b"caf\xe9\n")
        with self.assertRaises(ValueError) as raised:
            _run(str(self.path), "caf", "tea")
        self.assertIn("not valid UTF-8", str(raised.exception))
        self.assertEqual(self.path.read_bytes(), b"caf\xe9\n")

    def test_several_matches_need_replace_all(self):
        self.write(b"x = 1\nx = 1\n")
        with self.assertRaises(ValueError) as raised:
            _run(str(self.path), "x = 1", "x = 2")
        self.assertIn("Found 2 matches of the string to replace, but replace_all is False", str(raised.exception))
        self.assertEqual(_run(str(self.path), "x = 1", "x = 2", replace_all=True).split(" (")[1], "2 replacements)")
        self.assertEqual(self.path.read_bytes(), b"x = 2\nx = 2\n")

    def test_an_empty_old_str_creates_the_file_and_its_parents(self):
        target = Path(self.dir.name) / "new" / "dir" / "g.txt"
        self.assertTrue(_run(str(target), "", "fresh\n").startswith("Created "))
        self.assertEqual(target.read_bytes(), b"fresh\n")
        with self.assertRaises(ValueError) as raised:
            _run(str(target), "", "again\n")
        self.assertEqual(str(raised.exception), "Cannot create new file - file already exists.")

    def test_an_empty_old_str_fills_an_empty_file(self):
        self.write(b"  \n")
        _run(str(self.path), "", "body\n")
        self.assertEqual(self.path.read_bytes(), b"body\n")

    def test_straight_quotes_match_curly_ones_and_keep_the_file_style(self):
        self.write("print(“hello”)\nit’s\n".encode())
        _run(str(self.path), 'print("hello")', 'print("bye")')
        _run(str(self.path), "it's", "that's")
        self.assertEqual(self.path.read_text(encoding="utf-8"), "print(“bye”)\nthat’s\n")

    def test_deleting_a_line_removes_its_line_break(self):
        self.write(b"keep\r\ndrop me\r\nkeep too\r\n")
        _run(str(self.path), "drop me", "")
        self.assertEqual(self.path.read_bytes(), b"keep\r\nkeep too\r\n")

    def test_refusals(self):
        self.write(b"same\n")
        with self.assertRaises(ValueError) as raised:
            _run(str(self.path), "same", "same")
        self.assertIn("No changes to make", str(raised.exception))
        with self.assertRaises(ValueError) as raised:
            _run(str(self.path), "absent", "x")
        self.assertIn("String to replace not found", str(raised.exception))
        with self.assertRaises(FileNotFoundError):
            _run(str(Path(self.dir.name) / "missing.txt"), "a", "b")
        notebook = Path(self.dir.name) / "n.ipynb"
        notebook.write_text('{"cells": []}', encoding="utf-8")
        with self.assertRaises(ValueError) as raised:
            _run(str(notebook), "cells", "x")
        self.assertIn("edit.notebook_edit", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
