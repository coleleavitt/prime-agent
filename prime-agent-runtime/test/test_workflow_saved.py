"""Saved workflows: ``rlm.workflow.run(name=...)`` / ``start(name=...)`` and ``list()``
resolve ``.prime/workflows/`` then ``.claude/workflows/`` from the working directory up
to the repository root, then the user's agent and Claude Code directories."""

from __future__ import annotations

import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from rlm import workflow
from rlm import workflow_script as ws

from test_workflow_script import FakeBackend

SCRIPT = """
meta = {"name": "{name}", "description": "{description}", "whenToUse": "when asked"}


async def main(wf):
    return await wf.agent("say {name}")
"""


def write(path: Path, name: str, description: str = "") -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(SCRIPT.replace("{name}", name).replace("{description}", description), encoding="utf-8")
    return path


class SavedWorkflowTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name).resolve()
        self.home = root / "home"
        self.repo = self.home / "work" / "repo"
        self.cwd = self.repo / "app"
        self.cwd.mkdir(parents=True)
        (self.repo / ".git").mkdir()
        self.agent_dir = root / "agent"
        self.claude_dir = root / "claude"
        self.session_dir = root / "session"
        patcher = mock.patch.dict(
            os.environ,
            {
                "HOME": str(self.home),
                "RLM_SESSION_DIR": str(self.session_dir),
                "PRIME_AGENT_CODING_AGENT_DIR": str(self.agent_dir),
                "CLAUDE_CONFIG_DIR": str(self.claude_dir),
            },
        )
        patcher.start()
        self.addCleanup(patcher.stop)
        previous = Path.cwd()
        os.chdir(self.cwd)
        self.addCleanup(os.chdir, previous)

    def test_directories_follow_the_project_walk_then_the_user_dirs(self) -> None:
        # Above the repository root nothing is searched; the home directory never is.
        write(self.home / "work" / ".claude" / "workflows" / "outside.py", "outside")
        self.assertEqual(
            ws.saved_workflow_dirs(),
            [
                (self.cwd / ".prime/workflows", "project"),
                (self.cwd / ".claude/workflows", "project"),
                (self.repo / ".prime/workflows", "project"),
                (self.repo / ".claude/workflows", "project"),
                (self.agent_dir / "workflows", "user"),
                (self.claude_dir / "workflows", "user"),
            ],
        )

    def test_the_nearest_directory_wins_and_prime_before_claude(self) -> None:
        nearest = write(self.cwd / ".prime" / "workflows" / "review.py", "review", "nearest")
        write(self.cwd / ".claude" / "workflows" / "review.py", "review", "claude copy")
        write(self.repo / ".claude" / "workflows" / "review.py", "review", "repo copy")
        triage = write(self.repo / ".claude" / "workflows" / "triage.py", "triage", "repo triage")
        mine = write(self.claude_dir / "workflows" / "mine.py", "mine")
        write(self.agent_dir / "workflows" / "triage.py", "triage", "user triage")
        (self.claude_dir / "workflows" / "legacy.js").write_text("export const meta = {}\n")
        write(self.claude_dir / "workflows" / "_shared.py", "shared")

        listed = workflow.list()
        self.assertEqual(
            listed,
            [
                ws.SavedWorkflow(name="mine", path=mine, scope="user", description=None, when_to_use="when asked"),
                ws.SavedWorkflow(
                    name="review", path=nearest, scope="project", description="nearest", when_to_use="when asked"
                ),
                ws.SavedWorkflow(
                    name="triage", path=triage, scope="project", description="repo triage", when_to_use="when asked"
                ),
            ],
        )
        self.assertEqual(workflow.find_saved("review").path, nearest)

    def test_a_missing_name_says_which_workflows_exist(self) -> None:
        write(self.cwd / ".prime" / "workflows" / "review.py", "review")
        with self.assertRaisesRegex(ws.WorkflowError, r"no saved workflow 'nope'.*saved workflows: review"):
            workflow.find_saved("nope")
        with self.assertRaises(ws.WorkflowError):
            workflow.find_saved("../escape")
        with self.assertRaises(TypeError):
            ws._resolve_script(None, None)

    def test_meta_is_read_without_running_the_script(self) -> None:
        path = self.cwd / ".prime" / "workflows" / "boom.py"
        path.parent.mkdir(parents=True)
        path.write_text('raise SystemExit("ran")\nmeta = {"description": "never runs"}\n', encoding="utf-8")
        self.assertEqual(workflow.find_saved("boom").description, "never runs")

    async def test_run_by_name_runs_the_saved_script(self) -> None:
        write(self.cwd / ".claude" / "workflows" / "greet.py", "greet")
        backend = FakeBackend()
        result = await workflow.run(name="greet", backend=backend)
        self.assertEqual((result.status, result.name, result.result), ("completed", "greet", "answer to say greet"))
        # A bare saved name in the script position resolves the same way.
        again = await workflow.run("greet", backend=FakeBackend())
        self.assertEqual(again.result, "answer to say greet")

    async def test_start_by_name_runs_in_the_background(self) -> None:
        write(self.agent_dir / "workflows" / "bg.py", "bg")
        handle = workflow.start(name="bg", backend=FakeBackend(), notify=False)
        outcome = await handle.result()
        self.assertEqual((outcome.status, outcome.result), ("completed", "answer to say bg"))


if __name__ == "__main__":
    unittest.main()
