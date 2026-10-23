"""Tests for the code-review skill: argument parsing (Claude Code's ``Te()``),
diff gathering, the per-level workflow over a fake agent backend, ranking and
caps, persistence of the last level, ``report_outcomes`` and resume.

Run from the skill directory:
``PYTHONPATH=src ../../prime-agent-runtime/.venv/bin/python -m unittest discover -s tests``
"""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import code_review
from code_review import ReviewArgs, _parse_args
from rlm import workflow_script as ws

GIT_ENV = {
    "GIT_CONFIG_GLOBAL": "/dev/null",
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_AUTHOR_NAME": "t",
    "GIT_AUTHOR_EMAIL": "t@example.com",
    "GIT_COMMITTER_NAME": "t",
    "GIT_COMMITTER_EMAIL": "t@example.com",
    "TZ": "UTC",
}


def finding(file: str, line: int, summary: str, category: str = "correctness") -> dict:
    return {
        "file": file,
        "line": line,
        "summary": summary,
        "short_summary": summary[:60],
        "failure_scenario": f"when {summary}",
        "category": category,
    }


class ReviewBackend:
    """Answers finder, verifier, sweep and low-pass prompts like subagents would."""

    def __init__(self, refute: tuple[str, ...] = (), finders: dict[str, list[dict]] | None = None) -> None:
        self.refute = refute
        self.finders = finders or {
            "Angle A": [finding("app.py", 3, "inverted guard drops input")],
            "Angle B": [finding("app.py", 3, "inverted guard drops input"), finding("app.py", 5, "removed None check")],
            "Simplification": [finding("app.py", 7, "duplicated branch", "simplification")],
        }
        self.prompts: list[str] = []

    async def run(self, request: ws.AgentRequest) -> ws.AgentOutcome:
        prompt = request.prompt
        self.prompts.append(prompt)
        if "You verify one candidate" in prompt:
            verdict = "REFUTED" if any(word in prompt for word in self.refute) else "CONFIRMED"
            body = {"verdict": verdict, "evidence": "line 3"}
        elif "sweeping for gaps" in prompt:
            body = {"findings": [finding("app.py", 9, "lock scope shrank")]}
        elif "one careful pass" in prompt:
            body = {"findings": [finding("app.py", 3, "inverted guard drops input")]}
        else:
            body = {"findings": []}
            for marker, found in self.finders.items():
                if f"### {marker}" in prompt:
                    body = {"findings": found}
        return ws.AgentOutcome(text=f"```json\n{json.dumps(body)}\n```", agent_id="child")


class ParseArgsTests(unittest.TestCase):
    def test_levels_flags_targets_and_max_findings(self) -> None:
        self.assertEqual(_parse_args(""), ReviewArgs())
        self.assertEqual(
            _parse_args("high --fix #123"),
            ReviewArgs(level="high", target="123", fix=True),
        )
        self.assertEqual(
            _parse_args("--comment XHIGH `feature/x` --max-findings 20"),
            ReviewArgs(level="xhigh", target="feature/x", comment=True, max_findings=20),
        )
        self.assertEqual(_parse_args("src/app.py --max-findings=all").max_findings, "all")
        self.assertEqual(_parse_args("--max-findings default").max_findings, "default")
        self.assertEqual(
            _parse_args("med --max-findings zero"),
            ReviewArgs(target="med", unrecognized_level="med", max_findings_ignored=True),
        )
        self.assertTrue(_parse_args("--max-findings").max_findings_ignored)
        self.assertEqual(_parse_args("ultra --post 42"), ReviewArgs(target="42", post=True, ultra=True))
        self.assertEqual(_parse_args("main..feature").target, "main..feature")


class ReviewTestCase(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        self.repo = root / "repo"
        self.repo.mkdir()
        env = mock.patch.dict(
            os.environ,
            {
                **GIT_ENV,
                "RLM_SESSION_DIR": str(root / "session"),
                "PRIME_AGENT_CODING_AGENT_DIR": str(root / "agent"),
            },
        )
        env.start()
        self.addCleanup(env.stop)
        self.git("init", "-q", "-b", "main")
        (self.repo / "app.py").write_text("def f(x):\n    return x\n")
        self.git("add", "app.py")
        self.git("commit", "-q", "-m", "one")
        (self.repo / "app.py").write_text("def f(x):\n    if not x:\n        return None\n    return x\n")

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", *args], cwd=self.repo, check=True, capture_output=True, text=True
        ).stdout


class ReviewTests(ReviewTestCase):
    async def test_medium_finds_dedupes_verifies_and_ranks(self) -> None:
        backend = ReviewBackend(refute=("removed None check",))
        result = await code_review.review("medium", cwd=str(self.repo), backend=backend)
        self.assertEqual(result.level, "medium")
        self.assertEqual(result.scope, "uncommitted changes")
        self.assertEqual(
            [(f["index"], f["line"], f["category"], f["verdict"]) for f in result.findings],
            [(1, 3, "correctness", "CONFIRMED"), (2, 7, "simplification", "CONFIRMED")],
        )
        # 8 finders + 3 deduped candidates verified, no sweep at medium.
        self.assertEqual(len(backend.prompts), 8 + 3)
        self.assertEqual(result.candidates, 3)
        self.assertIn("+    if not x:", backend.prompts[0])
        table = repr(result)
        self.assertIn("| # | Location | Category | Verdict | Finding |", table)
        self.assertIn("| 1 | app.py:3 | correctness | CONFIRMED | inverted guard drops input |", table)
        self.assertIn("Scenario: when inverted guard drops input", table)
        self.assertIn("report_outcomes", table)
        saved = json.loads((Path(os.environ["RLM_SESSION_DIR"]) / "code-review" / f"{result.review_id}.json").read_text())
        self.assertEqual(saved["runId"], result.run_id)

    async def test_xhigh_runs_ten_angles_and_a_sweep(self) -> None:
        backend = ReviewBackend()
        result = await code_review.review("xhigh", cwd=str(self.repo), backend=backend)
        self.assertEqual(sum("### Angle" in prompt for prompt in backend.prompts), 5)
        self.assertEqual(sum("sweeping for gaps" in prompt for prompt in backend.prompts), 1)
        self.assertEqual([f["line"] for f in result.findings], [3, 5, 9, 7])
        self.assertIsNone(result.findings[2].get("verdict"))

    async def test_low_is_one_pass_and_ultra_maps_to_local_max(self) -> None:
        backend = ReviewBackend()
        low = await code_review.review("low", cwd=str(self.repo), backend=backend)
        self.assertEqual(len(backend.prompts), 1)
        self.assertEqual([f["line"] for f in low.findings], [3])

        ultra = await code_review.review("ultra", cwd=str(self.repo), backend=ReviewBackend())
        self.assertEqual(ultra.level, "max")
        self.assertIn("running the local max-effort review", ultra.notices[0])

    async def test_last_level_and_max_findings_persist(self) -> None:
        await code_review.review("high --max-findings 1", cwd=str(self.repo), backend=ReviewBackend())
        state = json.loads((Path(os.environ["PRIME_AGENT_CODING_AGENT_DIR"]) / "code-review.json").read_text())
        self.assertEqual(state, {"lastEffort": "high", "lastMaxFindings": 1})
        again = await code_review.review("", cwd=str(self.repo), backend=ReviewBackend())
        self.assertEqual((again.level, len(again.findings), again.dropped), ("high", 1, 2))
        self.assertTrue(any("reusing high" in notice for notice in again.notices))
        self.assertTrue(any("the limit set last time" in notice for notice in again.notices))
        capped = await code_review.review("--max-findings 99", cwd=str(self.repo), backend=ReviewBackend())
        self.assertTrue(any("at most 32 findings" in notice for notice in capped.notices))
        await code_review.review("--max-findings default", cwd=str(self.repo), backend=ReviewBackend())
        state = json.loads((Path(os.environ["PRIME_AGENT_CODING_AGENT_DIR"]) / "code-review.json").read_text())
        self.assertEqual(state, {"lastEffort": "high"})

    async def test_report_outcomes_adds_the_outcome_column(self) -> None:
        result = await code_review.review("medium --fix", cwd=str(self.repo), backend=ReviewBackend())
        self.assertIn("Applying fixes (--fix)", repr(result))
        updated = code_review.report_outcomes({1: "fixed", "#2": "skipped"})
        self.assertEqual([f.get("outcome") for f in updated.findings], ["fixed", "skipped", None])
        self.assertIn("| Outcome |", updated.table())
        self.assertEqual(code_review.load_review().findings[0]["outcome"], "fixed")
        with self.assertRaises(ValueError):
            code_review.report_outcomes({1: "done"})
        with self.assertRaises(KeyError):
            code_review.report_outcomes({99: "fixed"})

    async def test_an_empty_diff_reviews_nothing(self) -> None:
        self.git("checkout", "--", "app.py")
        backend = ReviewBackend()
        result = await code_review.review("high", cwd=str(self.repo), backend=backend)
        self.assertEqual(backend.prompts, [])
        self.assertIn("Nothing to review", repr(result))

    async def test_committed_changes_against_main_and_targets(self) -> None:
        self.git("checkout", "-q", "-b", "feature")
        self.git("commit", "-qam", "two")
        backend = ReviewBackend()
        result = await code_review.review("low", cwd=str(self.repo), backend=backend)
        self.assertEqual(result.scope, "main...HEAD")
        self.assertIn("+    if not x:", backend.prompts[0])
        on_path = await code_review.review("low app.py", cwd=str(self.repo), backend=ReviewBackend())
        self.assertEqual(on_path.scope, "changes to app.py")
        branch = await code_review.review("low feature", cwd=str(self.repo), backend=ReviewBackend())
        self.assertEqual(branch.scope, "the branch feature against main")
        with self.assertRaisesRegex(RuntimeError, "not a PR number"):
            await code_review.review("low nowhere", cwd=str(self.repo), backend=ReviewBackend())

    async def test_an_interrupted_review_resumes_from_the_journal(self) -> None:
        class Flaky(ReviewBackend):
            async def run(self, request):
                if "sweeping for gaps" in request.prompt:
                    raise RuntimeError("host went away")
                return await super().run(request)

        first = await code_review.review("max", cwd=str(self.repo), backend=Flaky())
        self.assertEqual(first.status, "failed")
        self.assertIn("resume_from", repr(first))
        backend = ReviewBackend()
        resumed = await code_review.review(resume_from=first.review_id, backend=backend)
        self.assertEqual(resumed.status, "completed")
        # Only the sweep runs live; every finder and verifier replays.
        self.assertEqual(len(backend.prompts), 1)
        self.assertIn("sweeping for gaps", backend.prompts[0])
        self.assertEqual(resumed.review_id, first.review_id)


if __name__ == "__main__":
    unittest.main()
