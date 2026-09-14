import copy
import hashlib
import json
from pathlib import Path
import unittest

from rlm import workflow

FIXTURE_DIR = Path(__file__).parents[2] / "scripts/fixtures"
SCHEMA_DIGESTS = {
    "workflow-v1.schema.json": "db3aa583523d4374e5ef455b1ada26744e0cd862d43384b86210e4c39bbf8663",
    "workflow-native-host-v1.schema.json": "08ade62e424d7dad199ca87b1a2da8eb57da71657a497f6793862fa1d73e1f6a",
}


def req():
    return {"protocol": workflow.REQUEST_PROTOCOL, "requestId": "r1", "nodeId": "n1", "prompt": "x", "model": None, "maxTurns": 1, "maxResultUtf8Bytes": 10, "drainTimeoutMs": 10, "tools": "none"}

def usage(finality="final"):
    return {"inputTokens": 1, "outputTokens": 1, "cacheReadTokens": 0, "cacheWriteTokens": 0, "totalTokens": 2, "costInput": None, "costOutput": None, "costCacheRead": None, "costCacheWrite": None, "costTotal": None, "completeness": "complete_host_observation", "finality": finality}

def done():
    return {"protocol": workflow.REPLY_PROTOCOL, "requestId": "r1", "nodeId": "n1", "resolvedModel": "p/m", "turnsStarted": 1, "durationMs": 1, "budgetExhausted": False, "budgetOvershootTokens": 0, "usage": usage(), "outcome": "completed", "stopReason": "completed", "result": {"text": "ok", "utf8Bytes": 2, "sha256": hashlib.sha256(b"ok").hexdigest()}, "error": None}

class WorkflowSchemaConformanceTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        raw = (FIXTURE_DIR / "workflow-native-host-v1.schema.json").read_bytes()
        cls.schema = json.loads(raw)

    def test_both_normative_schema_digests_and_mutants(self):
        for name, expected in SCHEMA_DIGESTS.items():
            with self.subTest(name=name):
                raw = (FIXTURE_DIR / name).read_bytes()
                self.assertEqual(hashlib.sha256(raw).hexdigest(), expected)
                mutant = bytearray(raw)
                mutant[-2] ^= 1
                self.assertNotEqual(hashlib.sha256(mutant).hexdigest(), expected)

    def test_normative_schema_constants_and_bounds(self):
        props = self.schema["$defs"]["runAgentRequest"]["properties"]
        self.assertEqual(props["protocol"]["const"], workflow.REQUEST_PROTOCOL)
        self.assertEqual(props["maxTurns"]["const"], 1)
        self.assertEqual(props["tools"]["const"], "none")
        self.assertEqual((props["prompt"]["minLength"], props["prompt"]["maxLength"]), (1, 262144))
        self.assertEqual((props["maxResultUtf8Bytes"]["minimum"], props["maxResultUtf8Bytes"]["maximum"]), (1, 1048576))
        self.assertEqual((props["drainTimeoutMs"]["minimum"], props["drainTimeoutMs"]["maximum"]), (1, 30000))
        outcomes = {v["properties"]["outcome"]["const"] for v in self.schema["$defs"]["runAgentReply"]["oneOf"]}
        self.assertEqual(outcomes, {"completed", "failed", "cancelled", "execution_unknown"})

    def test_rejects_request_mutations(self):
        patches = [
            {"protocol":"prime.workflow.run-agent/v2"}, {"maxTurns":0}, {"maxTurns":2}, {"tools":"all"},
            {"prompt":""}, {"prompt":"x"*262145}, {"softTokenBudget":0}, {"softTokenBudget":1000001},
            {"maxResultUtf8Bytes":0}, {"maxResultUtf8Bytes":1048577}, {"drainTimeoutMs":0}, {"drainTimeoutMs":30001},
        ]
        for patch in patches:
            with self.subTest(patch=patch), self.assertRaises(workflow.WorkflowWireError): workflow.validate_request({**req(), **patch})

    def test_rejects_digest_finality_and_unknown_mutations(self):
        mutations=[]
        for mutate in [
            lambda v: v.update(protocol="prime.workflow.run-agent-result/v2"),
            lambda v: v["usage"].update(finality="known_prefix"),
            lambda v: v.update(outcome="mystery"),
            lambda v: v["result"].update(utf8Bytes=1),
            lambda v: v["result"].update(sha256="0"*64),
        ]:
            value=done(); mutate(value); mutations.append(value)
        unknown={**done(), "outcome":"execution_unknown", "stopReason":"host_connection_lost", "result":None, "error":{"code":"EXECUTION_UNKNOWN","message":"lost"}, "usage":usage("known_prefix")}
        self.assertEqual(workflow.validate_reply(unknown, request=req())["outcome"], "execution_unknown")
        for mutate in [lambda v:v["usage"].update(finality="final"), lambda v:v.update(stopReason="provider_failed"), lambda v:v["error"].update(code="HOST_FAILED"), lambda v:v.update(result=done()["result"])]:
            value=copy.deepcopy(unknown); mutate(value); mutations.append(value)
        for value in mutations:
            with self.subTest(value=value), self.assertRaises(workflow.WorkflowWireError): workflow.validate_reply(value, request=req())

    def test_all_normative_terminal_variants(self):
        variants=self.schema["$defs"]["runAgentReply"]["oneOf"]
        for variant in variants:
            props=variant["properties"]; outcome=props["outcome"]["const"]
            if outcome != "failed": continue
            reason=props["stopReason"]["const"]; code=props["error"]["properties"]["code"]["const"]
            value={**done(), "outcome":"failed", "stopReason":reason, "result":None, "error":{"code":code,"message":"failure"}}
            self.assertEqual(workflow.validate_reply(value, request=req())["outcome"], "failed")
            value["error"]["code"] += "_MUTATED"
            with self.assertRaises(workflow.WorkflowWireError): workflow.validate_reply(value, request=req())
        cancelled={**done(), "outcome":"cancelled", "stopReason":"caller_aborted", "result":None, "error":None}
        self.assertEqual(workflow.validate_reply(cancelled, request=req())["outcome"], "cancelled")
        cancelled["result"]=done()["result"]
        with self.assertRaises(workflow.WorkflowWireError): workflow.validate_reply(cancelled, request=req())

if __name__ == "__main__": unittest.main()
