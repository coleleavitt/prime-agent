from __future__ import annotations

import os
import tempfile
import unittest
from typing import Any
from unittest import mock

import rlm


class WorktreeTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self._cwd = os.getcwd()
        self._dirs = tempfile.TemporaryDirectory()
        self.root = os.path.realpath(self._dirs.name)
        self.worktree = os.path.join(self.root, "repo", ".prime", "worktrees", "feat")
        os.makedirs(self.worktree)
        self.repo = os.path.join(self.root, "repo")
        os.chdir(self.repo)

    def tearDown(self) -> None:
        os.chdir(self._cwd)
        self._dirs.cleanup()

    def host(self, replies: dict[str, dict[str, Any]], sent: list[tuple[str, dict[str, Any]]]):
        async def fake(request_type: str, payload: dict[str, Any] | None = None) -> dict[str, Any]:
            sent.append((request_type, payload or {}))
            return replies[request_type]

        return mock.patch.object(rlm, "host_request", fake)

    async def test_enter_and_exit_move_the_kernel_with_the_session(self):
        sent: list[tuple[str, dict[str, Any]]] = []
        replies = {
            "worktree.enter": {"worktreePath": self.worktree, "worktreeBranch": "worktree-feat", "existed": False},
            "worktree.exit": {"action": "keep", "restoredCwd": self.repo},
        }
        with self.host(replies, sent):
            entered = await rlm.enter_worktree("feat")
            self.assertEqual(os.getcwd(), self.worktree)
            self.assertEqual(entered["worktreeBranch"], "worktree-feat")
            await rlm.rlm.exit_worktree("keep")
            self.assertEqual(os.getcwd(), self.repo)
        self.assertEqual(
            sent,
            [
                ("worktree.enter", {"name": "feat"}),
                ("worktree.exit", {"action": "keep", "discardChanges": False}),
            ],
        )

    async def test_a_path_is_sent_absolute_and_bad_arguments_never_reach_the_host(self):
        sent: list[tuple[str, dict[str, Any]]] = []
        replies = {"worktree.enter": {"worktreePath": self.worktree}}
        with self.host(replies, sent):
            await rlm.enter_worktree(path=os.path.join(".prime", "worktrees", "feat"))
            with self.assertRaises(ValueError):
                await rlm.enter_worktree("x", path="y")
            with self.assertRaises(ValueError):
                await rlm.exit_worktree("drop")
        self.assertEqual(sent, [("worktree.enter", {"path": self.worktree})])

    async def test_a_noop_exit_leaves_the_directory(self):
        sent: list[tuple[str, dict[str, Any]]] = []
        replies = {"worktree.exit": {"action": "none", "message": "nothing changed"}}
        with self.host(replies, sent):
            result = await rlm.exit_worktree("remove", discard_changes=True)
        self.assertEqual(result["action"], "none")
        self.assertEqual(os.getcwd(), self.repo)
        self.assertEqual(sent, [("worktree.exit", {"action": "remove", "discardChanges": True})])


if __name__ == "__main__":
    unittest.main()
