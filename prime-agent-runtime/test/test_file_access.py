from __future__ import annotations

import os
import sys
import tempfile
import unittest
import unittest.mock

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


class ReadStateTest(unittest.TestCase):
    """The kernel's read state behind the edit skill's read-before-edit and
    modified-since-read rules (Claude Code's readFileState)."""

    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.addCleanup(file_access.forget_read_state)
        file_access.forget_read_state()
        self.path = os.path.join(self.dir.name, "a.txt")
        with open(self.path, "w", encoding="utf-8") as handle:
            handle.write("one\n")
        patcher = unittest.mock.patch.dict(os.environ, {file_access.EDIT_READ_CHECK_ENV: ""})
        patcher.start()
        self.addCleanup(patcher.stop)

    def open_in_cell(self, mode: str) -> None:
        recorder, token = file_access.begin_cell()
        try:
            file_access._hook("open", (self.path, mode, 0))
        finally:
            file_access.reset_cell(token)
            recorder.close()

    def modify(self) -> None:
        with open(self.path, "a", encoding="utf-8") as handle:
            handle.write("two\n")
        stat = os.stat(self.path)
        os.utime(self.path, ns=(stat.st_atime_ns, stat.st_mtime_ns + 5_000_000_000))

    def test_a_file_changed_since_its_read_is_refused_until_read_again(self):
        self.assertIsNone(file_access.edit_refusal(self.path))  # never read: allowed by default
        self.open_in_cell("r")
        self.assertIsNone(file_access.edit_refusal(self.path))
        self.modify()
        self.assertEqual(file_access.edit_refusal(self.path), file_access.MODIFIED_SINCE_READ_MESSAGE)
        self.open_in_cell("r")
        self.assertIsNone(file_access.edit_refusal(self.path))

    def test_the_kernels_own_writes_count_as_known_content(self):
        self.open_in_cell("r")
        self.open_in_cell("w")
        self.modify()
        self.assertIsNone(file_access.edit_refusal(self.path))
        file_access.note_written(self.path)
        self.assertIsNone(file_access.edit_refusal(self.path))
        self.modify()
        self.assertEqual(file_access.edit_refusal(self.path), file_access.MODIFIED_SINCE_READ_MESSAGE)

    def test_the_modes(self):
        with unittest.mock.patch.dict(os.environ, {file_access.EDIT_READ_CHECK_ENV: "read"}):
            self.assertEqual(file_access.edit_refusal(self.path), file_access.NOT_READ_MESSAGE)
            # A file that does not exist yet needs no read.
            self.assertIsNone(file_access.edit_refusal(self.path + ".new"))
            self.open_in_cell("r")
            self.assertIsNone(file_access.edit_refusal(self.path))
        self.modify()
        with unittest.mock.patch.dict(os.environ, {file_access.EDIT_READ_CHECK_ENV: "off"}):
            self.assertIsNone(file_access.edit_refusal(self.path))
        self.assertEqual(file_access.edit_check_mode(), "stale")

    def test_opens_outside_a_cell_are_not_reads(self):
        file_access._hook("open", (self.path, "r", 0))
        with unittest.mock.patch.dict(os.environ, {file_access.EDIT_READ_CHECK_ENV: "read"}):
            self.assertEqual(file_access.edit_refusal(self.path), file_access.NOT_READ_MESSAGE)


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
