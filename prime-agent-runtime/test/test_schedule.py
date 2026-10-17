from __future__ import annotations

import asyncio
import importlib
import unittest
from unittest.mock import AsyncMock, patch

rlm_module = importlib.import_module("rlm")


class RlmCronTest(unittest.TestCase):
    def test_create_sends_claude_codes_fields(self) -> None:
        reply = {
            "id": "a1b2c3d4",
            "human_schedule": "Every 5 minutes",
            "recurring": True,
            "durable": False,
            "next_fire_at": 1,
            "text": "Scheduled recurring job a1b2c3d4 (Every 5 minutes).",
        }
        host_request = AsyncMock(return_value=reply)
        with patch.object(rlm_module, "host_request", host_request):
            result = asyncio.run(rlm_module.rlm.cron.create("*/5 * * * *", "check the deploy"))
        host_request.assert_awaited_once_with(
            "rlm.cron.create",
            {"cron": "*/5 * * * *", "prompt": "check the deploy", "recurring": True, "durable": False},
        )
        self.assertEqual(result, reply)

    def test_one_shot_durable_create_and_list_and_delete(self) -> None:
        host_request = AsyncMock(side_effect=[{"id": "x"}, {"jobs": [{"id": "x"}]}, {"id": "x", "deleted": True}])
        with patch.object(rlm_module, "host_request", host_request):
            asyncio.run(rlm_module.cron.create("30 14 17 10 *", "remind me", recurring=False, durable=True))
            jobs = asyncio.run(rlm_module.rlm.cron.list())
            deleted = asyncio.run(rlm_module.rlm.cron.delete("x"))
        self.assertEqual(jobs, [{"id": "x"}])
        self.assertTrue(deleted["deleted"])
        self.assertEqual(
            [call.args for call in host_request.await_args_list],
            [
                (
                    "rlm.cron.create",
                    {"cron": "30 14 17 10 *", "prompt": "remind me", "recurring": False, "durable": True},
                ),
                ("rlm.cron.list", {}),
                ("rlm.cron.delete", {"id": "x"}),
            ],
        )

    def test_bad_input_never_reaches_the_host(self) -> None:
        host_request = AsyncMock()
        with patch.object(rlm_module, "host_request", host_request):
            with self.assertRaises(ValueError):
                asyncio.run(rlm_module.rlm.cron.create("* * * * *", "  "))
            with self.assertRaises(TypeError):
                asyncio.run(rlm_module.rlm.cron.create("* * * * *", "x", recurring="yes"))  # type: ignore[arg-type]
            with self.assertRaises(ValueError):
                asyncio.run(rlm_module.rlm.cron.delete(""))
            with self.assertRaises(ValueError):
                asyncio.run(rlm_module.rlm.schedule_wakeup(float("inf"), "/loop x", "r"))
            with self.assertRaises(TypeError):
                asyncio.run(rlm_module.rlm.schedule_wakeup(True, "/loop x", "r"))  # type: ignore[arg-type]
        host_request.assert_not_awaited()


class RlmScheduleWakeupTest(unittest.TestCase):
    def test_wakeup_and_stop_payloads(self) -> None:
        host_request = AsyncMock(return_value={"text": "ok"})
        with patch.object(rlm_module, "host_request", host_request):
            asyncio.run(rlm_module.rlm.schedule_wakeup(270, "/loop check CI", "watching CI", noop=True))
            asyncio.run(rlm_module.schedule_wakeup(stop=True))
            asyncio.run(rlm_module.rlm.schedule_wakeup(prompt="/loop x"))
        self.assertEqual(
            [call.args for call in host_request.await_args_list],
            [
                (
                    "rlm.schedule_wakeup",
                    {"delay_seconds": 270.0, "prompt": "/loop check CI", "reason": "watching CI", "noop": True},
                ),
                ("rlm.schedule_wakeup", {"stop": True}),
                # Missing fields go through: the host answers Claude Code's error.
                ("rlm.schedule_wakeup", {"prompt": "/loop x"}),
            ],
        )


if __name__ == "__main__":
    unittest.main()
