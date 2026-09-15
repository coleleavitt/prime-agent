import unittest
from unittest.mock import AsyncMock, patch

from rlm import repl, workflow_v2


def definition():
    return {"protocol":"prime.workflow.definition/v2","nodes":[{"nodeId":"n","kind":"agent","prompt":"hi","dependsOn":[],"model":"m","maxTurns":1,"tools":"none","maxTokens":10}],"outputs":["n"],"budget":{"maxConcurrentAttempts":1,"maxTotalTokens":10,"semantics":"soft_admission"}}


def validate_request():
    return {"protocol":workflow_v2.REQUEST_PROTOCOL,"requestId":"r","action":"validate","definition":definition()}


def result():
    return {"protocol":workflow_v2.REPLY_PROTOCOL,"requestId":"r","action":"validate","valid":True,"definitionDigest":"sha256:"+"0"*64,"errors":[],"warnings":[]}


class WorkflowV2Test(unittest.IsolatedAsyncioTestCase):
    async def test_only_public_request_uses_exact_envelope(self):
        mock=AsyncMock(return_value={"status":"ok","result":result()})
        with patch.object(repl,"host_request",mock):
            self.assertEqual((await workflow_v2.request(request=validate_request()))["valid"],True)
        mock.assert_awaited_once_with({"type":"workflow.v2.request","request":validate_request()})

    async def test_missing_host_is_capability_unavailable(self):
        with patch.object(repl,"host_request",AsyncMock(side_effect=repl.HostRequestUnavailable("missing"))):
            with self.assertRaises(workflow_v2.CapabilityUnavailable) as caught:
                await workflow_v2.request(request=validate_request())
        self.assertEqual(caught.exception.code,"CAPABILITY_UNAVAILABLE")

    async def test_request_is_closed_and_utf8_bounded(self):
        bad=validate_request(); bad["extra"]=True
        with self.assertRaises(workflow_v2.WorkflowV2WireError):
            await workflow_v2.request(request=bad)
        bad=validate_request(); bad["definition"]["nodes"][0]["prompt"]="😀"*20000
        with self.assertRaises(workflow_v2.WorkflowV2WireError):
            await workflow_v2.request(request=bad)

    async def test_reply_is_closed_and_correlated(self):
        for mutation in (lambda x:x.update(extra=True), lambda x:x.update(requestId="other"), lambda x:x.update(action="status")):
            reply=result(); mutation(reply)
            with patch.object(repl,"host_request",AsyncMock(return_value={"status":"ok","result":reply})):
                with self.assertRaises(workflow_v2.WorkflowV2WireError):
                    await workflow_v2.request(request=validate_request())


    async def test_mutating_action_stays_unavailable_without_host_call(self):
        value={"protocol":workflow_v2.REQUEST_PROTOCOL,"requestId":"r","action":"create","definition":definition()}
        mock=AsyncMock()
        with patch.object(repl,"host_request",mock):
            with self.assertRaises(workflow_v2.CapabilityUnavailable):
                await workflow_v2.request(request=value)
        mock.assert_not_awaited()

    async def test_definition_graph_semantics_are_fail_closed(self):
        value=validate_request()
        value["definition"]["nodes"][0]["dependsOn"]=[{"nodeId":"n","require":"accepted"}]
        with self.assertRaises(workflow_v2.WorkflowV2WireError):
            await workflow_v2.request(request=value)

    async def test_non_ok_host_reply_is_unavailable_not_fallback(self):
        with patch.object(repl,"host_request",AsyncMock(return_value={"status":"error","error":"disabled"})):
            with self.assertRaises(workflow_v2.CapabilityUnavailable):
                await workflow_v2.request(request=validate_request())
