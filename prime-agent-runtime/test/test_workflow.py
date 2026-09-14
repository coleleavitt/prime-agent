import asyncio
import hashlib
import unittest
from unittest.mock import AsyncMock, patch

from rlm import repl, workflow


def request():
    return {"protocol": workflow.REQUEST_PROTOCOL, "requestId": "req-1", "nodeId": "node-1", "prompt": "hi", "model": None, "maxTurns": 1, "maxResultUtf8Bytes": 100, "drainTimeoutMs": 100, "tools": "none"}

def usage(finality="final"):
    return {"inputTokens": 1, "outputTokens": 1, "cacheReadTokens": 0, "cacheWriteTokens": 0, "totalTokens": 2, "costInput": None, "costOutput": None, "costCacheRead": None, "costCacheWrite": None, "costTotal": None, "completeness": "complete_host_observation", "finality": finality}

def completed():
    text = "ok"
    return {"protocol": workflow.REPLY_PROTOCOL, "requestId": "req-1", "nodeId": "node-1", "resolvedModel": "p/m", "turnsStarted": 1, "durationMs": 3, "budgetExhausted": False, "budgetOvershootTokens": 0, "usage": usage(), "outcome": "completed", "stopReason": "completed", "result": {"text": text, "utf8Bytes": 2, "sha256": hashlib.sha256(text.encode()).hexdigest()}, "error": None}

class WorkflowTest(unittest.IsolatedAsyncioTestCase):
    async def test_run_agent_uses_only_operation_and_closed_reply(self):
        mock = AsyncMock(return_value={"status": "ok", "result": completed()})
        with patch.object(repl, "host_request", mock):
            self.assertEqual((await workflow.run_agent(request()))["outcome"], "completed")
        mock.assert_awaited_once_with({"type": "workflow.run_agent", "request": request()}, cancel_on_cancel=True, drain_timeout_ms=100)

    async def test_missing_host_is_capability_unavailable(self):
        with patch.object(repl, "host_request", AsyncMock(side_effect=repl.HostRequestUnavailable("missing"))):
            with self.assertRaises(workflow.CapabilityUnavailable): await workflow.run_agent(request())

    async def test_drain_timeout_is_execution_unknown(self):
        with patch.object(repl, "host_request", AsyncMock(side_effect=repl.HostDrainTimeout("timeout"))):
            result = await workflow.run_agent(request())
        self.assertEqual(result["stopReason"], "drain_timeout")
        self.assertEqual(result["usage"]["finality"], "known_prefix")

    async def test_connection_loss_is_execution_unknown(self):
        with patch.object(repl, "host_request", AsyncMock(side_effect=repl.HostConnectionLost("lost"))):
            result = await workflow.run_agent(request())
        self.assertEqual(result["outcome"], "execution_unknown")
        self.assertEqual(result["usage"]["finality"], "known_prefix")

    def test_rejects_bad_digest_unknown_fields_and_finality(self):
        for mutate in [lambda x: x["result"].update(sha256="0"*64), lambda x: x.update(extra=True), lambda x: x["usage"].update(finality="known_prefix")]:
            value=completed(); mutate(value)
            with self.assertRaises(workflow.WorkflowWireError): workflow.validate_reply(value, request=request())

    async def test_host_request_cancel_shields_same_future_and_sends_once(self):
        old_loop, old_closed = repl._loop, repl._host_closed
        repl._loop = asyncio.get_running_loop(); repl._host_closed = False
        sent=[]
        try:
            with patch.object(repl, "_send", side_effect=lambda frame: sent.append(frame)):
                task=asyncio.create_task(repl.host_request({"type":"x"}, cancel_on_cancel=True, drain_timeout_ms=500))
                await asyncio.sleep(0); rid=sent[0]["id"]
                task.cancel(); await asyncio.sleep(0); task.cancel(); await asyncio.sleep(0)
                repl._resolve_host_reply(rid, {"status":"ok", "result":{}})
                self.assertEqual(await task, {"status":"ok", "result":{}})
            self.assertEqual([f["event"] for f in sent], ["host_request", "host_cancel"])
            self.assertEqual(sent[0]["id"], sent[1]["id"])
        finally:
            repl._loop, repl._host_closed = old_loop, old_closed
            repl._pending_host.clear()
    def test_rejects_budget_contradiction_and_completed_zero_turn(self):
        req = request()
        req["softTokenBudget"] = 1
        reply = completed()
        with self.assertRaisesRegex(workflow.WorkflowWireError, "budget semantics"):
            workflow.validate_reply(reply, request=req)
        reply["budgetExhausted"] = True
        reply["budgetOvershootTokens"] = 1
        reply["turnsStarted"] = 0
        with self.assertRaisesRegex(workflow.WorkflowWireError, "one started turn"):
            workflow.validate_reply(reply, request=req)
