"""Snapshot cost: a large namespace fits the host's snapshot window."""

from __future__ import annotations

import os
import sys
import tempfile
import time
import unittest

# `unittest discover -s test` puts test/ itself on sys.path.
from test_repl import SRC  # pyright: ignore[reportImplicitRelativeImport]

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


if __name__ == "__main__":
    unittest.main()
