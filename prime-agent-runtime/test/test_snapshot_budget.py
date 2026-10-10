"""Snapshot cost and the time budget: a large namespace fits the host's snapshot window, a capture
that runs out of time commits an explicit partial instead of being aborted, and the abort
interrupt never lands on unrelated kernel work."""

from __future__ import annotations

import json
import os
import sys
import tempfile
import time
import unittest

# `unittest discover -s test` puts test/ itself on sys.path.
from test_repl import SRC, ReplProcess, one, stream_text  # pyright: ignore[reportImplicitRelativeImport]

# The host's snapshot window (`SNAPSHOT_EXECUTION_TIMEOUT_MS`).
_HOST_SNAPSHOT_WINDOW_S = 5.0


class SnapshotCostTest(unittest.TestCase):
    def setUp(self) -> None:
        sys.path.insert(0, SRC)
        self.addCleanup(sys.path.remove, SRC)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.path = os.path.join(tmp.name, "kernel-state.dill")
        self.manifest_path = os.path.join(tmp.name, "kernel-state.json")

    def test_plain_data_namespace_fits_the_host_snapshot_window(self):
        # The shape behind the 5 s aborts: tens of MB of records, lists and text. dill's
        # pure-Python pickler needed well over the window for it; plain data must not.
        from rlm.repl import _restore_state, _snapshot_state

        ns = {
            "rows": [{"id": i, "name": f"item-{i}", "score": i * 0.5, "tags": ["a", "b"]} for i in range(400_000)],
            "pages": [f"<p>{i}</p>" * 2000 for i in range(200)],
            "matrix": [[float(j) for j in range(100)] for _ in range(20_000)],
        }
        started = time.monotonic()
        result = _snapshot_state(ns, self.path, self.manifest_path, 1 << 30, 1 << 30, False)
        elapsed = time.monotonic() - started
        self.assertEqual((result["saved"], result["skipped"]), (["matrix", "pages", "rows"], []))
        self.assertLess(elapsed, _HOST_SNAPSHOT_WINDOW_S / 2)
        restored: dict[str, object] = {}
        self.assertEqual(_restore_state(restored, self.path, None, 1 << 30, 1 << 30)["failed"], [])
        self.assertEqual(restored, ns)

    def test_main_functions_reference_globals_instead_of_copying_them(self):
        # A cell function used to carry a by-value copy of every global it reads: twenty helpers
        # over one dataset wrote the dataset twenty-one times.
        from rlm.repl import _snapshot_state

        main = sys.modules["__main__"].__dict__
        names = ["snapshot_cost_data"] + [f"snapshot_cost_fn{i}" for i in range(20)]
        self.addCleanup(lambda: [main.pop(name, None) for name in names])
        exec(  # noqa: S102 - the functions must be defined in __main__, like a cell's
            "snapshot_cost_data = list(range(200_000))\n"
            + "".join(f"def snapshot_cost_fn{i}():\n    return len(snapshot_cost_data)\n" for i in range(20)),
            main,
        )
        ns = {name: main[name] for name in names}
        data_only = _snapshot_state({"snapshot_cost_data": main["snapshot_cost_data"]}, self.path, self.manifest_path, 1 << 30, 1 << 30, False)
        result = _snapshot_state(ns, self.path, self.manifest_path, 1 << 30, 1 << 30, False)
        self.assertEqual(sorted(result["saved"]), sorted(names))
        self.assertLess(result["bytes"], data_only["bytes"] * 2)


class SnapshotBudgetTest(unittest.TestCase):
    def setUp(self) -> None:
        sys.path.insert(0, SRC)
        self.addCleanup(sys.path.remove, SRC)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.path = os.path.join(tmp.name, "kernel-state.dill")
        self.manifest_path = os.path.join(tmp.name, "kernel-state.json")

    def _snap(self, ns: dict[str, object], budget_ms: int | None = None) -> dict[str, object]:
        from rlm.repl import _snapshot_state

        return _snapshot_state(ns, self.path, self.manifest_path, 1 << 20, 1 << 20, False, budget_ms=budget_ms)

    def test_exhausted_budget_commits_an_explicit_partial(self):
        from rlm.repl import _restore_state

        self.assertEqual(self._snap({"a": 1, "b": "old"})["stale"], [])
        # Nothing fits a spent budget: the names the previous snapshot holds keep its value,
        # a new name is not persisted, and both say so instead of the capture failing.
        result = self._snap({"a": 2, "b": "old", "c": 3}, budget_ms=0)
        kept = "snapshot time budget ran out; kept the previous snapshot's value"
        lost = "snapshot time budget ran out; not persisted"
        stale = [{"name": "a", "reason": kept}, {"name": "b", "reason": kept}, {"name": "c", "reason": lost}]
        self.assertEqual(
            {key: result[key] for key in ("saved", "skipped", "stale")},
            {"saved": ["a", "b"], "skipped": [{"name": "c", "reason": lost}], "stale": stale},
        )
        with open(self.manifest_path) as fh:
            manifest = json.load(fh)
        self.assertEqual((manifest["savedNames"], manifest["stale"]), (["a", "b"], stale))
        restored: dict[str, object] = {}
        self.assertEqual(_restore_state(restored, self.path), {"restored": ["a", "b"], "failed": []})
        self.assertEqual(restored, {"a": 1, "b": "old"})

    def test_exhausted_budget_without_a_previous_snapshot_persists_nothing_stale(self):
        result = self._snap({"a": 1}, budget_ms=0)
        lost = [{"name": "a", "reason": "snapshot time budget ran out; not persisted"}]
        self.assertEqual((result["saved"], result["skipped"], result["stale"]), ([], lost, lost))

    def test_budget_is_checked_inside_one_slow_value(self):
        # One value alone can outlast the budget: the writer stops it mid-dump.
        from unittest import mock

        from rlm import repl

        clock = [0.0]

        class Slow:
            def __reduce__(self):
                clock[0] += 10.0
                return (list, ([b"x" * 200_000],))

        with mock.patch.object(repl, "_snapshot_clock", lambda: clock[0]):
            result = self._snap({"slow": [Slow(), Slow()], "z": 1}, budget_ms=5_000)
        lost = "snapshot time budget ran out; not persisted"
        self.assertEqual(
            (result["saved"], result["stale"]), ([], [{"name": "slow", "reason": lost}, {"name": "z", "reason": lost}])
        )

    def test_no_budget_snapshots_everything(self):
        result = self._snap({"a": 1, "b": 2})
        self.assertEqual((result["saved"], result["stale"]), (["a", "b"], []))


class SnapshotInterruptTargetTest(unittest.TestCase):
    """The host interrupts a snapshot it timed out; that interrupt is for the snapshot alone."""

    def setUp(self) -> None:
        self.repl = ReplProcess()
        self.addCleanup(self.repl.close)
        self.repl.ready()

    def test_snapshot_interrupt_never_raises_into_background_work(self):
        # A detached task blocks the loop while the snapshot is active but has not started yet:
        # aborting the snapshot must cancel the snapshot, not kill the user's background work.
        code = (
            "import asyncio, time\n"
            "from rlm import repl as _r\n"
            "outcome = []\n"
            "async def background():\n"
            "    while not str(_r._active['rid']).startswith('snap'):\n"
            "        await asyncio.sleep(0)\n"
            "    print('blocking', flush=True)\n"
            "    try:\n"
            "        time.sleep(3)\n"
            "        outcome.append('finished')\n"
            "    except KeyboardInterrupt:\n"
            "        outcome.append('interrupted')\n"
            "job = asyncio.ensure_future(background())\n"
        )
        self.assertEqual(one(self.repl.execute("c1", code), "done"), {"event": "done", "id": "c1", "status": "ok"})
        with tempfile.TemporaryDirectory() as tmp:
            self.repl.send(
                {
                    "type": "snapshot",
                    "id": "snap1",
                    "path": os.path.join(tmp, "s.dill"),
                    "manifest_path": os.path.join(tmp, "s.json"),
                }
            )
            events: list[dict[str, object]] = []
            while "blocking" not in stream_text(events, "stdout"):
                events.append(self.repl.read_event())
            self.repl.send({"type": "interrupt", "id": "snap1"})
            self.repl.until_done("snap1")
        events = self.repl.execute("c2", "await job\noutcome")
        self.assertEqual(one(events, "result"), {"event": "result", "id": "c2", "text": "['finished']"})


if __name__ == "__main__":
    unittest.main()
