"""refine.preview() and refine.run(plan_id=...) (upstream #899): approve a plan before it applies."""

from __future__ import annotations

import asyncio
import importlib
import sys
import types
import unittest


class RefinePreviewTest(unittest.TestCase):
    def setUp(self):
        self.requests: list[tuple[str, dict | None]] = []
        self.replies: dict[str, object] = {}
        saved_rlm = sys.modules.get("rlm")
        saved_refine = sys.modules.pop("refine", None)

        def restore():
            sys.modules.pop("refine", None)
            if saved_refine is not None:
                sys.modules["refine"] = saved_refine
            if saved_rlm is None:
                sys.modules.pop("rlm", None)
            else:
                sys.modules["rlm"] = saved_rlm

        self.addCleanup(restore)
        fake = types.ModuleType("rlm")

        async def host_request(request_type, payload=None):
            self.requests.append((request_type, payload))
            return self.replies[request_type]

        fake.host_request = host_request
        sys.modules["rlm"] = fake
        self.refine = importlib.import_module("refine")

    def test_preview_plans_through_the_host_and_returns_its_plan(self):
        plan = {"plan_id": "refine_1", "summary": "s", "edits": []}
        self.replies["refine.preview"] = plan
        self.assertEqual(asyncio.run(self.refine.preview("focus", global_=True)), plan)
        self.assertEqual(self.requests, [("refine.preview", {"instructions": "focus", "global": True})])

    def test_run_pins_a_previewed_plan(self):
        self.replies["refine.run"] = {"scheduled": True}
        self.assertEqual(asyncio.run(self.refine.run(plan_id="refine_1")), {"scheduled": True})
        self.assertEqual(self.requests, [("refine.run", {"plan_id": "refine_1"})])

    def test_arguments_are_type_checked_before_any_request(self):
        for call in (
            lambda: self.refine.preview(5),
            lambda: self.refine.preview(global_="yes"),
            lambda: self.refine.run(plan_id=7),
        ):
            with self.assertRaises(TypeError):
                asyncio.run(call())
        self.assertEqual(self.requests, [])


if __name__ == "__main__":
    unittest.main()
