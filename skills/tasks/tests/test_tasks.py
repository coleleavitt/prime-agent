"""Tests for the tasks skill: the task list's API, its dependency fields, the
in_progress warning, and persistence through ``$RLM_SESSION_DIR/tasks.json``.

Run from the skill directory:
``PYTHONPATH=src ../../prime-agent-runtime/.venv/bin/python -m unittest discover -s tests``
"""

from __future__ import annotations

import contextlib
import importlib
import io
import json
import os
import pickle
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

import tasks


class TasksTestCase(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._dir.cleanup)
        self.session_dir = Path(self._dir.name) / "session-artifacts" / "s1"
        patcher = mock.patch.dict(os.environ, {"RLM_SESSION_DIR": str(self.session_dir)})
        patcher.start()
        self.addCleanup(patcher.stop)
        self.stderr = io.StringIO()
        redirect = contextlib.redirect_stderr(self.stderr)
        redirect.__enter__()
        self.addCleanup(redirect.__exit__, None, None, None)

    @property
    def store_file(self) -> Path:
        return self.session_dir / "tasks.json"

    def stored(self) -> dict[str, Any]:
        return json.loads(self.store_file.read_text(encoding="utf-8"))


class CreateGetTests(TasksTestCase):
    async def test_create_returns_a_pending_task_with_monotonic_ids(self) -> None:
        first = await tasks.create("Add the migration", "Old files lack a key", active_form="Adding the migration")
        second = await tasks.create("Write the test")
        for task in (first, second):
            self.assertTrue(task.pop("created_at"))
            self.assertTrue(task.pop("updated_at"))
        self.assertEqual(
            first,
            {
                "id": "1",
                "subject": "Add the migration",
                "description": "Old files lack a key",
                "status": "pending",
                "active_form": "Adding the migration",
                "owner": None,
                "blocks": [],
                "blocked_by": [],
                "metadata": {},
            },
        )
        self.assertEqual(second["id"], "2")
        self.assertIsNone(second["active_form"])

    async def test_ids_are_never_reused_after_a_delete(self) -> None:
        await tasks.create("a")
        await tasks.create("b")
        await tasks.update("2", status="deleted")
        third = await tasks.create("c")
        self.assertEqual(third["id"], "3")

    async def test_get_accepts_str_int_and_hash_ids(self) -> None:
        await tasks.create("a")
        for task_id in ("1", 1, "#1"):
            self.assertEqual((await tasks.get(task_id))["subject"], "a")

    async def test_get_unknown_id_and_bad_arguments_raise(self) -> None:
        with self.assertRaisesRegex(ValueError, "no task #9"):
            await tasks.get("9")
        with self.assertRaises(ValueError):
            await tasks.get("one")
        with self.assertRaises(TypeError):
            await tasks.get(True)
        with self.assertRaisesRegex(ValueError, "subject must not be empty"):
            await tasks.create("  ")
        with self.assertRaises(TypeError):
            await tasks.create("a", metadata={"x": object()})
        self.assertFalse(self.store_file.exists())


class UpdateTests(TasksTestCase):
    async def test_update_changes_fields_and_merges_metadata(self) -> None:
        await tasks.create("a", metadata={"keep": 1, "drop": 2})
        updated = await tasks.update(
            "1",
            status="in_progress",
            subject="A",
            description="details",
            active_form="Doing A",
            owner="worker-1",
            metadata={"drop": None, "new": "x"},
        )
        self.assertEqual(
            {key: updated[key] for key in ("status", "subject", "description", "active_form", "owner", "metadata")},
            {
                "status": "in_progress",
                "subject": "A",
                "description": "details",
                "active_form": "Doing A",
                "owner": "worker-1",
                "metadata": {"keep": 1, "new": "x"},
            },
        )
        self.assertNotIn("warnings", updated)
        cleared = await tasks.update("1", owner="", active_form="")
        self.assertIsNone(cleared["owner"])
        self.assertIsNone(cleared["active_form"])

    async def test_invalid_status_is_refused_without_writing(self) -> None:
        await tasks.create("a")
        before = self.store_file.read_bytes()
        with self.assertRaisesRegex(ValueError, "status must be one of"):
            await tasks.update("1", status="done")  # type: ignore[arg-type]
        self.assertEqual(self.store_file.read_bytes(), before)

    async def test_a_deleted_task_cannot_be_updated(self) -> None:
        await tasks.create("a")
        await tasks.update("1", status="deleted")
        self.assertEqual((await tasks.get("1"))["status"], "deleted")
        with self.assertRaisesRegex(ValueError, "task #1 is deleted"):
            await tasks.update("1", status="pending")


class DependencyTests(TasksTestCase):
    async def test_dependencies_are_recorded_on_both_tasks(self) -> None:
        for subject in ("schema", "migrate", "test", "deploy"):
            await tasks.create(subject)
        await tasks.update("1", add_blocks=["2", 3])
        await tasks.update("4", add_blocked_by="3")
        self.assertEqual((await tasks.get("1"))["blocks"], ["2", "3"])
        self.assertEqual((await tasks.get("2"))["blocked_by"], ["1"])
        self.assertEqual((await tasks.get("3"))["blocked_by"], ["1"])
        self.assertEqual((await tasks.get("3"))["blocks"], ["4"])
        self.assertEqual((await tasks.get("4"))["blocked_by"], ["3"])

    async def test_list_reports_only_open_blockers(self) -> None:
        await tasks.create("a")
        await tasks.create("b")
        await tasks.update("2", add_blocked_by="1")
        self.assertEqual((await tasks.list())[1]["blocked_by"], ["1"])
        await tasks.update("1", status="completed")
        self.assertEqual((await tasks.list())[1]["blocked_by"], [])
        self.assertEqual((await tasks.get("2"))["blocked_by"], ["1"])

    async def test_self_blocking_cycles_and_unknown_ids_are_refused(self) -> None:
        await tasks.create("a")
        await tasks.create("b")
        await tasks.update("1", add_blocks="2")
        before = self.store_file.read_bytes()
        with self.assertRaisesRegex(ValueError, "cannot block itself"):
            await tasks.update("1", add_blocks="1")
        with self.assertRaisesRegex(ValueError, "dependency cycle"):
            await tasks.update("2", add_blocks="1")
        with self.assertRaisesRegex(ValueError, "no task #7"):
            await tasks.update("1", add_blocked_by=["7"])
        self.assertEqual(self.store_file.read_bytes(), before)

    async def test_deleting_a_task_drops_it_from_other_tasks_dependencies(self) -> None:
        await tasks.create("a")
        await tasks.create("b")
        await tasks.update("2", add_blocked_by="1")
        await tasks.update("1", status="deleted")
        self.assertEqual((await tasks.get("2"))["blocked_by"], [])
        with self.assertRaisesRegex(ValueError, "task #1 is deleted"):
            await tasks.update("2", add_blocked_by="1")


class WarningTests(TasksTestCase):
    async def test_a_second_in_progress_task_warns_but_is_applied(self) -> None:
        await tasks.create("a")
        await tasks.create("b")
        first = await tasks.update("1", status="in_progress")
        self.assertNotIn("warnings", first)
        second = await tasks.update("2", status="in_progress")
        self.assertEqual(second["status"], "in_progress")
        self.assertEqual(len(second["warnings"]), 1)
        self.assertIn("#1 is already in_progress", second["warnings"][0])
        self.assertIn("tasks: warning: #1 is already in_progress", self.stderr.getvalue())
        self.assertEqual([row["status"] for row in await tasks.list()], ["in_progress", "in_progress"])
        self.assertNotIn("warnings", self.stored()["tasks"][1])

    async def test_starting_a_task_with_open_blockers_warns(self) -> None:
        await tasks.create("a")
        await tasks.create("b")
        await tasks.update("2", add_blocked_by="1")
        started = await tasks.update("2", status="in_progress")
        self.assertEqual(started["warnings"], ["#2 is still blocked by open task #1"])


class ListTests(TasksTestCase):
    async def test_list_orders_by_id_keeps_completed_and_hides_deleted(self) -> None:
        for subject in ("one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"):
            await tasks.create(subject)
        await tasks.update("1", status="completed")
        await tasks.update("2", status="deleted")
        await tasks.update("10", owner="me")
        rows = await tasks.list()
        self.assertIsInstance(rows, list)
        self.assertEqual([row["id"] for row in rows], ["1", "3", "4", "5", "6", "7", "8", "9", "10"])
        self.assertEqual(
            rows[0],
            {"id": "1", "subject": "one", "status": "completed", "owner": None, "blocked_by": []},
        )
        self.assertEqual(rows[-1]["owner"], "me")

    async def test_list_repr_is_a_checklist(self) -> None:
        self.assertEqual(repr(await tasks.list()), "Tasks: none")
        await tasks.create("Add the migration", active_form="Adding the migration")
        await tasks.create("Write the test")
        await tasks.create("Ship it")
        await tasks.create("Dropped")
        await tasks.update("1", status="completed")
        await tasks.update("2", status="in_progress", active_form="Writing the test")
        await tasks.update("3", add_blocked_by="2", owner="me")
        await tasks.update("4", status="deleted")
        rows = await tasks.list()
        expected = "\n".join(
            [
                "Tasks: 1/3 completed",
                "  [x] #1 Add the migration",
                "  [>] #2 Writing the test",
                "  [ ] #3 Ship it (blocked by #2; owner: me)",
            ]
        )
        self.assertEqual(repr(rows), expected)
        self.assertEqual(str(rows), expected)
        self.assertEqual(rows.text, expected)

    async def test_list_result_pickles_like_a_list(self) -> None:
        await tasks.create("a")
        rows = await tasks.list()
        restored = pickle.loads(pickle.dumps(rows))
        self.assertEqual(restored, rows)
        self.assertEqual(repr(restored), repr(rows))


class PersistenceTests(TasksTestCase):
    async def test_the_list_survives_a_module_reload(self) -> None:
        await tasks.create("a", metadata={"k": "v"})
        await tasks.create("b")
        await tasks.update("2", add_blocked_by="1", status="in_progress")
        before = await tasks.get("2")
        # A kernel restart re-imports the skill with no in-memory state.
        reloaded = importlib.reload(tasks)
        self.assertEqual(await reloaded.get("2"), before)
        self.assertEqual((await reloaded.get("1"))["metadata"], {"k": "v"})
        self.assertEqual((await reloaded.create("c"))["id"], "3")

    async def test_the_store_file_is_versioned_json_written_atomically(self) -> None:
        await tasks.create("a")
        stored = self.stored()
        self.assertEqual((stored["version"], stored["next_id"]), (1, 2))
        self.assertEqual([task["id"] for task in stored["tasks"]], ["1"])
        self.assertEqual(sorted(path.name for path in self.session_dir.iterdir()), ["tasks.json"])

    async def test_every_change_records_the_write_marks_reads_do_not(self) -> None:
        await tasks.create("a")
        first = self.stored()
        self.assertEqual(first["writes"], 1)
        await tasks.update("1", status="in_progress")
        await tasks.list()
        await tasks.get("1")
        second = self.stored()
        self.assertEqual(second["writes"], 2)
        self.assertGreaterEqual(second["last_write_at"], first["last_write_at"])
        self.assertGreaterEqual(second["last_write_at"], second["tasks"][0]["updated_at"])

    async def test_a_failed_write_leaves_the_previous_file(self) -> None:
        await tasks.create("a")
        before = self.store_file.read_bytes()
        with mock.patch.object(tasks.os, "replace", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                await tasks.create("b")
        self.assertEqual(self.store_file.read_bytes(), before)
        self.assertEqual(sorted(path.name for path in self.session_dir.iterdir()), ["tasks.json"])

    async def test_a_corrupt_file_is_reported_not_overwritten(self) -> None:
        self.session_dir.mkdir(parents=True)
        self.store_file.write_text("{not json", encoding="utf-8")
        with self.assertRaisesRegex(RuntimeError, "is not valid JSON"):
            await tasks.list()
        with self.assertRaisesRegex(RuntimeError, "is not valid JSON"):
            await tasks.create("a")
        self.assertEqual(self.store_file.read_text(encoding="utf-8"), "{not json")


class InMemoryFallbackTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        patcher = mock.patch.dict(os.environ, {"RLM_SESSION_DIR": ""})
        patcher.start()
        self.addCleanup(patcher.stop)
        importlib.reload(tasks)
        self.addCleanup(importlib.reload, tasks)

    async def test_without_a_session_dir_the_list_lives_in_memory_and_says_so(self) -> None:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            await tasks.create("a")
            await tasks.create("b")
            rows = await tasks.list()
        self.assertEqual([row["id"] for row in rows], ["1", "2"])
        self.assertTrue(rows.text.startswith("Tasks: 0/2 completed (in memory only: no session directory)"))
        self.assertEqual(stderr.getvalue().count("lives in kernel memory only"), 1)


if __name__ == "__main__":
    sys.exit(unittest.main())
