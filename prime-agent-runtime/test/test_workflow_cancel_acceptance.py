"""Acceptance matrix for Workflow V1 cancellation and terminal races."""
import asyncio
import unittest
from unittest.mock import patch

from rlm import repl


class WorkflowCancelAcceptanceTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.old_loop = repl._loop
        self.old_closed = repl._host_closed
        repl._loop = asyncio.get_running_loop()
        repl._host_closed = False
        repl._pending_host.clear()
        self.sent = []
        self.send_patch = patch.object(repl, "_send", side_effect=self.sent.append)
        self.send_patch.start()

    async def asyncTearDown(self):
        self.send_patch.stop()
        for future in repl._pending_host.values():
            if not future.done():
                future.cancel()
        repl._pending_host.clear()
        repl._loop = self.old_loop
        repl._host_closed = self.old_closed

    async def _admit(self, *, timeout=100):
        task = asyncio.create_task(
            repl.host_request(
                {"type": "workflow.run_agent"},
                cancel_on_cancel=True,
                drain_timeout_ms=timeout,
            )
        )
        await asyncio.sleep(0)
        request = next(frame for frame in reversed(self.sent) if frame["event"] == "host_request")
        return task, request["id"]

    async def test_pre_admission_channel_loss_fails_without_wire_request(self):
        repl._host_closed = True
        with self.assertRaises(repl.HostRequestUnavailable):
            await repl.host_request(
                {"type": "workflow.run_agent"}, cancel_on_cancel=True, drain_timeout_ms=10
            )
        self.assertEqual(self.sent, [])
        self.assertEqual(repl._pending_host, {})

    async def test_repeated_python_cancel_shields_same_future_and_sends_exactly_one_cancel(self):
        task, rid = await self._admit()
        settlement = repl._pending_host[rid]
        task.cancel()
        await asyncio.sleep(0)
        task.cancel()
        await asyncio.sleep(0)
        self.assertIs(repl._pending_host[rid], settlement)
        self.assertFalse(settlement.cancelled())
        repl._resolve_host_reply(rid, {"status": "ok", "result": {"outcome": "cancelled"}})
        self.assertEqual((await asyncio.wait_for(task, 0.25))["result"]["outcome"], "cancelled")
        self.assertEqual(
            [(frame["event"], frame["id"]) for frame in self.sent],
            [("host_request", rid), ("host_cancel", rid)],
        )
        self.assertNotIn(rid, repl._pending_host)

    async def test_terminal_before_cancel_wins_without_cancel_frame(self):
        task, rid = await self._admit()
        repl._resolve_host_reply(rid, {"status": "ok", "result": {"outcome": "completed"}})
        await asyncio.sleep(0)
        task.cancel()
        result = await asyncio.wait_for(task, 0.25)
        self.assertEqual(result["result"]["outcome"], "completed")
        self.assertEqual([frame["event"] for frame in self.sent], ["host_request"])

    async def test_simultaneous_terminal_and_cancel_has_one_terminal_and_at_most_one_cancel(self):
        task, rid = await self._admit()
        task.cancel()
        repl._resolve_host_reply(rid, {"status": "ok", "result": {"outcome": "cancelled"}})
        result = await asyncio.wait_for(task, 0.25)
        self.assertEqual(result["result"]["outcome"], "cancelled")
        self.assertLessEqual(sum(frame["event"] == "host_cancel" for frame in self.sent), 1)
        self.assertNotIn(rid, repl._pending_host)

    async def test_channel_loss_settles_all_requests_and_late_callbacks_cannot_turn_unknown_into_success(self):
        first, first_id = await self._admit()
        second, second_id = await self._admit()
        repl._fail_pending_host_requests()
        with self.assertRaises(repl.HostConnectionLost):
            await asyncio.wait_for(first, 0.25)
        with self.assertRaises(repl.HostConnectionLost):
            await asyncio.wait_for(second, 0.25)
        repl._resolve_host_reply(first_id, {"status": "ok", "result": {"outcome": "completed"}})
        repl._resolve_host_reply(second_id, {"status": "ok", "result": {"outcome": "completed"}})
        await asyncio.sleep(0)
        self.assertEqual(repl._pending_host, {})

    async def test_stuck_host_iterator_has_bounded_cleanup_and_late_success_is_dropped(self):
        task, rid = await self._admit(timeout=1)
        task.cancel()
        with self.assertRaises(repl.HostDrainTimeout):
            await asyncio.wait_for(task, 0.25)
        self.assertNotIn(rid, repl._pending_host)
        repl._resolve_host_reply(rid, {"status": "ok", "result": {"outcome": "completed"}})
        await asyncio.sleep(0)
        self.assertNotIn(rid, repl._pending_host)

    async def test_cancel_and_reply_callbacks_are_isolated_by_exact_request_id(self):
        first, first_id = await self._admit()
        second, second_id = await self._admit()
        first.cancel()
        await asyncio.sleep(0)
        self.assertFalse(repl._pending_host[second_id].done())
        repl._resolve_host_reply(first_id, {"status": "ok", "result": {"outcome": "cancelled"}})
        repl._resolve_host_reply(second_id, {"status": "ok", "result": {"outcome": "completed"}})
        self.assertEqual((await asyncio.wait_for(first, 0.25))["result"]["outcome"], "cancelled")
        self.assertEqual((await asyncio.wait_for(second, 0.25))["result"]["outcome"], "completed")
        cancel_ids = [frame["id"] for frame in self.sent if frame["event"] == "host_cancel"]
        self.assertEqual(cancel_ids, [first_id])


class WorkflowCancelAdmissionRaceTest(unittest.IsolatedAsyncioTestCase):
    async def test_cancel_before_send_failure_leaves_no_pending_future(self):
        old_loop, old_closed = repl._loop, repl._host_closed
        repl._loop, repl._host_closed = asyncio.get_running_loop(), False
        repl._pending_host.clear()
        try:
            with patch.object(repl, "_send", side_effect=BrokenPipeError("channel lost")):
                with self.assertRaises(BrokenPipeError):
                    await repl.host_request(
                        {"type": "workflow.run_agent"},
                        cancel_on_cancel=True,
                        drain_timeout_ms=10,
                    )
            self.assertEqual(repl._pending_host, {})
        finally:
            repl._pending_host.clear()
            repl._loop, repl._host_closed = old_loop, old_closed
