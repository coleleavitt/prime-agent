from __future__ import annotations

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))

from rlm import file_access  # noqa: E402
from test_repl import ReplProcess, one  # noqa: E402


class CellFileAccessTest(unittest.TestCase):
    def test_a_recorder_splits_reads_and_writes_and_dedupes(self):
        recorder = file_access.CellFileAccess()
        recorder.record("/a", write=False)
        recorder.record("/a", write=False)
        recorder.record("/b", write=True)
        self.assertEqual(recorder.close(), {"reads": ["/a"], "writes": ["/b"]})
        # Closed: later opens (a detached cell's) are not recorded.
        recorder.record("/c", write=False)
        self.assertEqual(recorder.close(), {"reads": ["/a"], "writes": ["/b"]})

    def test_an_empty_recorder_has_no_field(self):
        self.assertIsNone(file_access.CellFileAccess().close())

    def test_each_list_is_bounded(self):
        recorder = file_access.CellFileAccess()
        for index in range(file_access.CELL_PATH_LIMIT + 10):
            recorder.record(f"/f{index}", write=False)
        field = recorder.close()
        assert field is not None
        self.assertEqual(len(field["reads"]), file_access.CELL_PATH_LIMIT)
        self.assertEqual(field["reads"][0], "/f0")

    def test_write_intent_follows_mode_then_flags(self):
        self.assertFalse(file_access.is_write("r", 0))
        self.assertFalse(file_access.is_write("rb", 0))
        self.assertTrue(file_access.is_write("r+", 0))
        self.assertTrue(file_access.is_write("a", 0))
        self.assertTrue(file_access.is_write(None, os.O_WRONLY))
        self.assertFalse(file_access.is_write(None, os.O_RDONLY))

    def test_interpreter_and_bytecode_paths_are_skipped(self):
        self.assertIsNone(file_access.recorded_path(3))
        self.assertIsNone(file_access.recorded_path("/x/__pycache__/m.cpython-313.pyc"))
        self.assertIsNone(file_access.recorded_path("/v/lib/python3.13/site-packages/m.py"))
        self.assertEqual(file_access.recorded_path("/work/notes.txt"), "/work/notes.txt")


class DoneFrameFileAccessTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.repl = ReplProcess()
        self.addCleanup(self.repl.close)
        self.repl.ready()

    def test_the_done_frame_lists_the_files_a_cell_opened(self):
        read_path = os.path.join(self.dir.name, "read.txt")
        written_path = os.path.join(self.dir.name, "written.txt")
        with open(read_path, "w", encoding="utf-8") as handle:
            handle.write("hello\n")
        code = "\n".join(
            [
                "from pathlib import Path",
                f"text = Path({read_path!r}).read_text()",
                f"open({written_path!r}, 'w').write('x')",
                "import json, csv",  # imports are never recorded
                f"import os; os.listdir({self.dir.name!r})",  # nor directory listings
            ]
        )
        done = one(self.repl.execute("files", code), "done")
        self.assertEqual(done["status"], "ok")
        self.assertEqual(
            done["fileAccess"],
            {"reads": [read_path], "writes": [written_path]},
        )
        plain = one(self.repl.execute("files-none", "1 + 1"), "done")
        self.assertNotIn("fileAccess", plain)

    def test_a_task_the_cell_spawned_records_into_the_cell(self):
        read_path = os.path.join(self.dir.name, "task.txt")
        with open(read_path, "w", encoding="utf-8") as handle:
            handle.write("task\n")
        code = "\n".join(
            [
                "import asyncio",
                "async def reader():",
                f"    return open({read_path!r}).read()",
                "await asyncio.create_task(reader())",
            ]
        )
        done = one(self.repl.execute("task-files", code), "done")
        self.assertEqual(done["fileAccess"], {"reads": [read_path]})


if __name__ == "__main__":
    unittest.main()
