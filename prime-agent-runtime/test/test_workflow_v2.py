import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

from rlm import repl, workflow_v2


def definition():
    return {
        "protocol": "prime.workflow.definition/v2",
        "nodes": [
            {
                "nodeId": "n",
                "kind": "agent",
                "prompt": "hi",
                "dependsOn": [],
                "model": "m",
                "maxTurns": 1,
                "tools": "none",
                "maxTokens": 10,
            }
        ],
        "outputs": ["n"],
        "budget": {
            "maxConcurrentAttempts": 1,
            "maxTotalTokens": 10,
            "semantics": "soft_admission",
        },
    }


def validate_request():
    return {
        "protocol": workflow_v2.REQUEST_PROTOCOL,
        "requestId": "r",
        "action": "validate",
        "definition": definition(),
    }


def result():
    return {
        "protocol": workflow_v2.REPLY_PROTOCOL,
        "requestId": "r",
        "action": "validate",
        "valid": True,
        "definitionDigest": "sha256:" + "0" * 64,
        "errors": [],
        "warnings": [],
    }


class WorkflowV2Test(unittest.IsolatedAsyncioTestCase):
    async def test_only_public_request_uses_exact_envelope(self):
        mock = AsyncMock(return_value={"status": "ok", "result": result()})
        with patch.object(repl, "host_request", mock):
            self.assertEqual(
                (await workflow_v2.request(request=validate_request()))["valid"], True
            )
        mock.assert_awaited_once_with(
            {"type": "workflow.v2.request", "request": validate_request()}
        )

    async def test_missing_host_is_capability_unavailable(self):
        with patch.object(
            repl,
            "host_request",
            AsyncMock(side_effect=repl.HostRequestUnavailable("missing")),
        ):
            with self.assertRaises(workflow_v2.CapabilityUnavailable) as caught:
                await workflow_v2.request(request=validate_request())
        self.assertEqual(caught.exception.code, "CAPABILITY_UNAVAILABLE")

    async def test_request_is_closed_and_utf8_bounded(self):
        bad = validate_request()
        bad["extra"] = True
        with self.assertRaises(workflow_v2.WorkflowV2WireError):
            await workflow_v2.request(request=bad)
        bad = validate_request()
        bad["definition"]["nodes"][0]["prompt"] = "😀" * 20000
        with self.assertRaises(workflow_v2.WorkflowV2WireError):
            await workflow_v2.request(request=bad)

    async def test_reply_is_closed_and_correlated(self):
        for mutation in (
            lambda x: x.update(extra=True),
            lambda x: x.update(requestId="other"),
            lambda x: x.update(action="status"),
        ):
            reply = result()
            mutation(reply)
            with patch.object(
                repl,
                "host_request",
                AsyncMock(return_value={"status": "ok", "result": reply}),
            ):
                with self.assertRaises(workflow_v2.WorkflowV2WireError):
                    await workflow_v2.request(request=validate_request())

    async def test_mutating_action_stays_unavailable_without_host_call(self):
        value = {
            "protocol": workflow_v2.REQUEST_PROTOCOL,
            "requestId": "r",
            "action": "create",
            "definition": definition(),
        }
        mock = AsyncMock()
        with patch.object(repl, "host_request", mock):
            with self.assertRaises(workflow_v2.CapabilityUnavailable):
                await workflow_v2.request(request=value)
        mock.assert_not_awaited()

    async def test_definition_graph_semantics_are_fail_closed(self):
        value = validate_request()
        value["definition"]["nodes"][0]["dependsOn"] = [
            {"nodeId": "n", "require": "accepted"}
        ]
        with self.assertRaises(workflow_v2.WorkflowV2WireError):
            await workflow_v2.request(request=value)

    async def test_budget_must_cover_each_node_attempt(self):
        for action in ("validate", "create"):
            bad = definition()
            bad["budget"]["maxTotalTokens"] = 9
            request = {
                "protocol": workflow_v2.REQUEST_PROTOCOL,
                "requestId": "r",
                "action": action,
                "definition": bad,
            }
            with self.assertRaises(workflow_v2.WorkflowV2WireError):
                await workflow_v2.request(request=request)

    async def test_non_ok_host_reply_is_unavailable_not_fallback(self):
        with patch.object(
            repl,
            "host_request",
            AsyncMock(return_value={"status": "error", "error": "disabled"}),
        ):
            with self.assertRaises(workflow_v2.CapabilityUnavailable):
                await workflow_v2.request(request=validate_request())


class WorkflowV2SchemaPackagingTest(unittest.TestCase):
    def setUp(self):
        workflow_v2._SCHEMA = None
        self.addCleanup(setattr, workflow_v2, "_SCHEMA", None)

    def test_the_wheel_ships_the_schema_where_the_package_reads_it(self):
        root = Path(workflow_v2.__file__).resolve().parents[2]
        pyproject = tomllib.loads((root / "pyproject.toml").read_text(encoding="utf-8"))
        wheel = pyproject["tool"]["hatch"]["build"]["targets"]["wheel"]
        packaged = workflow_v2._SCHEMA_PATHS[0]
        self.assertEqual(
            wheel.get("force-include", {}).get("schemas/workflow-v2.schema.json"),
            packaged.relative_to(packaged.parents[2]).as_posix(),
        )
        self.assertEqual(packaged.parents[2], Path(workflow_v2.__file__).resolve().parents[1])
        self.assertTrue((root / "schemas" / "workflow-v2.schema.json").is_file())

    def test_the_packaged_copy_wins_and_the_checkout_copy_is_the_fallback(self):
        with tempfile.TemporaryDirectory() as tmp:
            packaged = Path(tmp) / "packaged.json"
            checkout = Path(tmp) / "checkout.json"
            checkout.write_text('{"$defs": {"which": "checkout"}}', encoding="utf-8")
            with patch.object(workflow_v2, "_SCHEMA_PATHS", (packaged, checkout)):
                self.assertEqual(workflow_v2._schema()["$defs"], {"which": "checkout"})
                workflow_v2._SCHEMA = None
                packaged.write_text('{"$defs": {"which": "packaged"}}', encoding="utf-8")
                self.assertEqual(workflow_v2._schema()["$defs"], {"which": "packaged"})

    def test_a_missing_schema_is_capability_unavailable(self):
        with tempfile.TemporaryDirectory() as tmp:
            missing = (Path(tmp) / "a.json", Path(tmp) / "b.json")
            with patch.object(workflow_v2, "_SCHEMA_PATHS", missing):
                with self.assertRaises(workflow_v2.CapabilityUnavailable):
                    workflow_v2._schema()
