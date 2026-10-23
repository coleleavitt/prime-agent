"""The workflow script API (``rlm.workflow.run``/``start``) over a fake agent backend:
journal keys pinned to Claude Code's, pipeline/parallel semantics, schema repair,
prefix-replay resume, caps, nesting and the background completion notice."""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from rlm import repl, workflow
from rlm import workflow_script as ws

SCHEMA = {
    "type": "object",
    "properties": {"value": {"type": "integer"}},
    "required": ["value"],
    "additionalProperties": False,
}


class FakeBackend:
    """Answers each prompt by a rule; records calls; can hold calls on gates."""

    def __init__(self, answer=None) -> None:
        self.calls: list[ws.AgentRequest] = []
        self.answer = answer or (lambda request: f"answer to {request.prompt.splitlines()[-1]}")
        self.gates: dict[str, asyncio.Event] = {}
        self.events: list[str] = []
        self.running = 0
        self.peak = 0

    @staticmethod
    def task(request: ws.AgentRequest) -> str:
        # The script's own prompt is the last paragraph before any schema text.
        body = request.prompt.split("\n\n", 1)[1]
        return body.split("\n\nYour final message must end", 1)[0]

    async def run(self, request: ws.AgentRequest) -> ws.AgentOutcome:
        self.calls.append(request)
        task = self.task(request) if request.prompt.startswith("You are a workflow subagent") else request.prompt
        self.events.append(f"start:{task}")
        self.running += 1
        self.peak = max(self.peak, self.running)
        try:
            gate = self.gates.get(task)
            if gate is not None:
                await gate.wait()
            await asyncio.sleep(0)
            answer = self.answer(request)
        finally:
            self.running -= 1
        self.events.append(f"end:{task}")
        if isinstance(answer, ws.AgentOutcome):
            return answer
        return ws.AgentOutcome(text=answer, agent_id=f"child-{len(self.calls)}")


class WorkflowTestCase(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.session_dir = Path(directory.name)
        patcher = mock.patch.dict(
            os.environ, {"RLM_SESSION_DIR": str(self.session_dir), "PRIME_AGENT_CODING_AGENT_DIR": str(self.session_dir / "agent")}
        )
        patcher.start()
        self.addCleanup(patcher.stop)


class JournalKeyTests(unittest.TestCase):
    def test_keys_match_claude_codes_chained_sha256(self) -> None:
        # Golden values from Claude Code 2.1.296's own Hr/No functions run under node.
        schema = {
            "type": "object",
            "properties": {
                "zeta": {"type": "string"},
                "alpha": {"type": "array", "items": {"type": "integer"}},
                "10": {"type": "number"},
                "9": {"type": "boolean"},
            },
            "required": ["alpha"],
        }
        k1 = ws.journal_key("", "first prompt", None)
        k2 = ws.journal_key(
            k1,
            "second prompt — ünïcode",
            {"schema": schema, "model": "anthropic/claude-x", "effort": "high", "isolation": None},
        )
        k3 = ws.journal_key(
            k2,
            "third",
            {"isolation": "worktree", "schema": {"type": "object", "properties": {"x": {"type": "number", "minimum": 1.0}}}},
        )
        self.assertEqual(
            (k1, k2, k3),
            (
                "v2:862653f9d6e6cf65fd5fd67790392337b21a1f550f409e8fa13adf5f5bd1b15a",
                "v2:16de7b7bcd04c740381b6876e22d4380594919618cf8ada99d6fd332747f8c6b",
                "v2:065d471de2127e944c23452cee07ab37cb2e5cef981466c0cc9d625146fe1f3e",
            ),
        )
        self.assertEqual(
            ws.canonical_options({"schema": schema, "model": "anthropic/claude-x", "effort": "high"}),
            '{"effort":"high","model":"anthropic/claude-x","schema":{"properties":{"9":{"type":"boolean"},'
            '"10":{"type":"number"},"alpha":{"items":{"type":"integer"},"type":"array"},'
            '"zeta":{"type":"string"}},"required":["alpha"],"type":"object"}}',
        )

    def test_display_only_options_do_not_change_the_key(self) -> None:
        self.assertEqual(ws.canonical_options({"label": "x", "phase": "y"}), "{}")
        self.assertEqual(ws.canonical_options(None), "{}")


class SemanticsTests(WorkflowTestCase):
    async def test_agent_returns_text_and_the_run_records_it(self) -> None:
        backend = FakeBackend()

        async def main(wf):
            wf.phase("Only")
            return await wf.agent("say hi", label="greeter")

        result = await workflow.run(main, backend=backend, name="hello")
        self.assertEqual(result.status, "completed")
        self.assertEqual(result.result, "answer to say hi")
        self.assertEqual([(r.label, r.phase, r.state) for r in result.agents], [("greeter", "Only", "done")])
        self.assertTrue(backend.calls[0].name.startswith(f"wf-{result.run_id[3:11]}-1-greeter"))
        record = json.loads((result.run_dir / "run.json").read_text())
        self.assertEqual((record["status"], record["agents"], record["name"]), ("completed", 1, "hello"))
        self.assertIn("async def main(wf)", (result.run_dir / "script.py").read_text())
        lines = [json.loads(line) for line in result.journal_path.read_text().splitlines()]
        self.assertEqual([line["type"] for line in lines], ["started", "result"])
        self.assertEqual(lines[1]["result"], "answer to say hi")

    async def test_pipeline_has_no_barrier_between_stages(self) -> None:
        backend = FakeBackend()
        backend.gates["first b"] = asyncio.Event()

        async def main(wf):
            async def release_when_a_verified(previous, item, index):
                value = await wf.agent(f"second {item}")
                if item == "a":
                    backend.gates["first b"].set()
                return value

            return await wf.pipeline(["a", "b"], lambda item: wf.agent(f"first {item}"), release_when_a_verified)

        result = await workflow.run(main, backend=backend)
        self.assertEqual(result.result, ["answer to second a", "answer to second b"])
        # Item a reached stage two while item b was still held in stage one.
        self.assertLess(backend.events.index("end:second a"), backend.events.index("end:first b"))

    async def test_a_failing_stage_drops_only_its_item(self) -> None:
        backend = FakeBackend()
        seen = []

        async def main(wf):
            def explode(previous, item):
                if item == 2:
                    raise RuntimeError("boom")
                return previous

            def record(previous, item, index):
                seen.append(index)
                return previous

            return await wf.pipeline([1, 2, 3], lambda item: item * 10, explode, record)

        result = await workflow.run(main, backend=backend)
        self.assertEqual(result.result, [10, None, 30])
        self.assertEqual(sorted(seen), [0, 2])
        self.assertTrue(any("pipeline item 1 dropped: RuntimeError: boom" in line for line in result.logs))

    async def test_parallel_is_a_barrier_that_never_raises(self) -> None:
        backend = FakeBackend()

        async def main(wf):
            def bad():
                raise ValueError("no")

            return await wf.parallel([lambda: wf.agent("one"), bad, lambda: 3])

        result = await workflow.run(main, backend=backend)
        self.assertEqual(result.result, ["answer to one", None, 3])

    async def test_a_failed_child_resolves_to_none_and_journals_failed(self) -> None:
        backend = FakeBackend(lambda request: ws.AgentOutcome(text=None, agent_id="c1", error="error"))

        async def main(wf):
            return await wf.agent("doomed")

        result = await workflow.run(main, backend=backend)
        self.assertIsNone(result.result)
        self.assertEqual((result.agents[0].state, result.agents[0].error), ("failed", "error"))
        types = [json.loads(line)["type"] for line in result.journal_path.read_text().splitlines()]
        self.assertEqual(types, ["started", "failed"])

    async def test_concurrency_is_capped(self) -> None:
        backend = FakeBackend()

        async def main(wf):
            return await wf.parallel([lambda i=i: wf.agent(f"task {i}") for i in range(9)])

        result = await workflow.run(main, backend=backend, concurrency=3)
        self.assertEqual(len(result.result), 9)
        self.assertLessEqual(backend.peak, 3)
        self.assertEqual(ws._default_concurrency(), max(1, min(16, (os.cpu_count() or 4) - 2)))

    async def test_item_caps_nesting_and_module_hooks(self) -> None:
        backend = FakeBackend()

        async def child(wf):
            await wf.workflow(lambda wf: None)

        async def main(wf):
            with self.assertRaises(ws.WorkflowError):
                await wf.parallel([lambda: 1] * (ws.MAX_ITEMS + 1))
            with self.assertRaisesRegex(ws.WorkflowError, "one level"):
                await wf.workflow(child)
            # Module-level hooks reach the running script's context.
            workflow.log("from the module")
            self.assertEqual(workflow.get_args(), {"x": 1})
            inner = await wf.workflow(lambda inner_wf: inner_wf.args, args="nested-args")
            return inner, await workflow.agent("via module")

        result = await workflow.run(main, backend=backend, args={"x": 1})
        self.assertEqual(result.status, "completed", result.error)
        self.assertEqual(result.result, ("nested-args", "answer to via module"))
        self.assertIn("from the module", result.logs)
        with self.assertRaisesRegex(ws.WorkflowError, "only inside a workflow script"):
            workflow.phase("outside")

    async def test_a_script_error_fails_the_run(self) -> None:
        async def main(wf):
            raise KeyError("missing")

        result = await workflow.run(main, backend=FakeBackend())
        self.assertEqual((result.status, result.error), ("failed", "KeyError: 'missing'"))

    async def test_script_files_and_bad_agent_options(self) -> None:
        path = self.session_dir / "flow.py"
        path.write_text(
            "meta = {'name': 'from-file'}\n"
            "async def main(wf):\n"
            "    return await wf.agent(wf.args)\n"
        )
        result = await workflow.run(str(path), args="file task", backend=FakeBackend())
        self.assertEqual((result.name, result.result), ("from-file", "answer to file task"))

        async def bad(wf):
            for options in ({"effort": "huge"}, {"isolation": "remote"}, {"agent_type": "x"}, {"schema": {"type": "string"}}):
                with self.assertRaises((ValueError, ws.WorkflowError)):
                    wf.agent("x", **options)
            return "ok"

        self.assertEqual((await workflow.run(bad, backend=FakeBackend())).result, "ok")

    async def test_budget_refuses_agents_once_spent(self) -> None:
        backend = FakeBackend(lambda request: ws.AgentOutcome(text="x", agent_id="c", tokens=600))

        async def main(wf):
            await wf.agent("one")
            await wf.agent("two")
            self.assertEqual((wf.budget.spent(), wf.budget.remaining()), (1200, 0))
            wf.agent("three")

        result = await workflow.run(main, backend=backend, budget=1000)
        self.assertEqual(result.status, "failed")
        self.assertIn("WorkflowBudgetExceeded", result.error)


class SchemaTests(WorkflowTestCase):
    async def test_structured_answers_are_parsed_and_validated(self) -> None:
        backend = FakeBackend(lambda request: 'done.\n```json\n{"value": 7}\n```')

        async def main(wf):
            return await wf.agent("count", schema=SCHEMA)

        result = await workflow.run(main, backend=backend)
        self.assertEqual(result.result, {"value": 7})
        self.assertIn('"required"', backend.calls[0].prompt)

    async def test_a_mismatch_asks_a_repair_agent_for_the_corrected_value(self) -> None:
        def answer(request):
            if "Do not redo the agent's work" in request.prompt:
                self.assertIn("$.value: expected integer, got string", request.prompt)
                return '{"value": 3}'
            return '{"value": "three"}'

        backend = FakeBackend(answer)

        async def main(wf):
            return await wf.agent("count", schema=SCHEMA)

        result = await workflow.run(main, backend=backend)
        self.assertEqual(result.result, {"value": 3})
        self.assertEqual(len(backend.calls), 2)
        self.assertTrue(backend.calls[1].name.endswith("-fix1"))

    async def test_repairs_give_up_after_the_limit(self) -> None:
        backend = FakeBackend(lambda request: "no json here")

        async def main(wf):
            return await wf.agent("count", schema=SCHEMA)

        result = await workflow.run(main, backend=backend)
        self.assertIsNone(result.result)
        self.assertEqual(len(backend.calls), 1 + ws.MAX_SCHEMA_REPAIRS)
        self.assertIn("does not match the schema", result.agents[0].error)


class ResumeTests(WorkflowTestCase):
    async def test_unchanged_prefix_replays_and_the_first_change_runs_live(self) -> None:
        prompts = ["one", "two", "three"]

        async def main(wf):
            return [await wf.agent(prompt) for prompt in prompts]

        first = await workflow.run(main, backend=FakeBackend())
        self.assertEqual(first.cached, 0)

        same = FakeBackend()
        again = await workflow.run(main, backend=same, resume_from=first.run_id)
        self.assertEqual((again.run_id, again.cached, len(same.calls)), (first.run_id, 3, 0))
        self.assertEqual(again.result, first.result)

        prompts[1] = "TWO"
        edited = FakeBackend()
        resumed = await workflow.run(main, backend=edited, resume_from=first.run_id)
        self.assertEqual([r.cached for r in resumed.agents], [True, False, False])
        # Everything after the first changed call runs live, even an unchanged prompt.
        self.assertEqual([FakeBackend.task(call) for call in edited.calls], ["TWO", "three"])

    async def test_an_interrupted_run_resumes_past_its_in_flight_agents(self) -> None:
        async def main(wf):
            return await wf.parallel([lambda p=p: wf.agent(p) for p in ("a", "b", "c")])

        first = await workflow.run(main, backend=FakeBackend())
        # Rewrite the journal as a kill would leave it: b started but never finished.
        lines = [json.loads(line) for line in first.journal_path.read_text().splitlines()]
        key_b = first.agents[1].key
        kept = [line for line in lines if not (line["key"] == key_b and line["type"] == "result")]
        first.journal_path.write_text("".join(json.dumps(line) + "\n" for line in kept))

        backend = FakeBackend()
        resumed = await workflow.run(main, backend=backend, resume_from=first.run_id)
        self.assertEqual([r.cached for r in resumed.agents], [True, False, True])
        self.assertEqual([FakeBackend.task(call) for call in backend.calls], ["b"])

    async def test_a_failed_agent_reruns_and_breaks_the_prefix(self) -> None:
        outcomes = iter([None, "ok-a"])

        async def main(wf):
            return [await wf.agent("a"), await wf.agent("b")]

        first = await workflow.run(
            main,
            backend=FakeBackend(lambda request: ws.AgentOutcome(text=next(outcomes, "x"), agent_id="c")),
        )
        self.assertEqual(first.result[0], None)
        backend = FakeBackend()
        await workflow.run(main, backend=backend, resume_from=first.run_id)
        self.assertEqual([FakeBackend.task(call) for call in backend.calls], ["a", "b"])

    async def test_resume_needs_a_journal(self) -> None:
        async def main(wf):
            return None

        with self.assertRaisesRegex(ValueError, "run id"):
            await workflow.run(main, resume_from="not-a-run")
        with self.assertRaisesRegex(ws.WorkflowError, "no journal"):
            await workflow.run(main, resume_from="wf_missing123")
        self.assertEqual(workflow.runs(), [])


class BackgroundTests(WorkflowTestCase):
    async def test_start_notifies_on_the_monitor_lane(self) -> None:
        sent = []

        async def host_request(payload, **_kwargs):
            sent.append(payload)
            return {}

        async def main(wf):
            return await wf.agent("background job")

        with mock.patch.object(repl, "host_request", host_request):
            handle = workflow.start(main, backend=FakeBackend(), name="bg")
            self.assertIn("WorkflowHandle", repr(handle))
            outcome = await workflow.result(handle.run_id)
            await asyncio.sleep(0)
        self.assertEqual(outcome.result, "answer to background job")
        self.assertEqual(len(sent), 1)
        event = sent[0]
        self.assertEqual(
            (event["type"], event["monitorId"], event["description"], event["end"]),
            ("monitor.event", handle.run_id, "workflow bg", {"reason": "completed"}),
        )
        self.assertTrue(event["lines"][0].startswith(f"workflow bg ({handle.run_id}) completed: 1 agents"))
        self.assertEqual(workflow.status(handle.run_id)["status"], "completed")
        self.assertEqual(workflow.runs()[0]["runId"], handle.run_id)
        self.assertEqual([line["type"] for line in workflow.load_journal(handle.run_id)], ["started", "result"])

    async def test_size_guideline_reads_settings_and_warns_once(self) -> None:
        agent_dir = Path(os.environ["PRIME_AGENT_CODING_AGENT_DIR"])
        agent_dir.mkdir(parents=True)
        self.assertEqual(workflow.size_guideline(), ("medium", 10))
        (agent_dir / "settings.json").write_text(json.dumps({"workflowSizeGuideline": "small"}))
        self.assertEqual(workflow.size_guideline(), ("small", 5))

        async def main(wf):
            return await wf.parallel([lambda i=i: wf.agent(f"t{i}") for i in range(7)])

        result = await workflow.run(main, backend=FakeBackend())
        warnings = [line for line in result.logs if "size guideline" in line]
        self.assertEqual(len(warnings), 1)


class RlmBackendIsolationTests(unittest.IsolatedAsyncioTestCase):
    """An isolated agent is a host-cut worktree child, deleted once it settles."""

    async def run_backend(self, isolation):
        import rlm

        calls: list[tuple[str, object]] = []
        handle = mock.Mock(rlm_child_id="c1")

        async def spawn(prompt, **kwargs):
            calls.append(("spawn", kwargs))
            return handle

        async def collect(target, timeout_ms=None):
            calls.append(("collect", target is handle))
            return [mock.Mock(settled=True, status="done", answer_text="ok", rlm_child_id="c1")]

        async def delete_subagent(target):
            calls.append(("delete", target is handle))

        with (
            mock.patch.object(rlm, "spawn", spawn),
            mock.patch.object(rlm, "collect", collect),
            mock.patch.object(rlm, "delete_subagent", delete_subagent),
        ):
            outcome = await ws.RlmBackend().run(ws.AgentRequest(prompt="p", name="wf-1", isolation=isolation))
        return outcome, calls

    async def test_an_isolated_agent_spawns_in_a_worktree_and_is_released(self):
        outcome, calls = await self.run_backend("worktree")
        self.assertEqual(outcome.text, "ok")
        self.assertEqual(
            calls,
            [("spawn", {"name": "wf-1", "isolation": "worktree"}), ("collect", True), ("delete", True)],
        )

    async def test_a_plain_agent_is_not_deleted(self):
        _, calls = await self.run_backend(None)
        self.assertEqual(calls, [("spawn", {"name": "wf-1"}), ("collect", True)])


class SchemaSubsetTests(unittest.TestCase):
    def test_validation_and_extraction(self) -> None:
        from rlm import workflow_schema as schema

        spec = {
            "type": "object",
            "properties": {
                "items": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}, "maxItems": 2},
                "n": {"type": ["integer", "null"], "minimum": 0},
            },
            "required": ["items"],
        }
        schema.check_schema(spec)
        self.assertEqual(schema.validate({"items": ["a"], "n": None}, spec), [])
        self.assertEqual(
            schema.validate({"items": ["a", "c", "b"], "n": -1}, spec),
            ["$.items: more than 2 items", "$.items[1]: must be one of \"a\", \"b\"", "$.n: below the minimum 0"],
        )
        with self.assertRaisesRegex(schema.SchemaError, "unsupported schema keyword"):
            schema.check_schema({"type": "object", "properties": {"x": {"pattern": "a"}}})
        with self.assertRaisesRegex(schema.SchemaError, "undeclared"):
            schema.check_schema({"type": "object", "properties": {}, "required": ["x"]})
        self.assertEqual(schema.extract_json('prose {"a": {"b": 1}} trailing'), {"a": {"b": 1}})
        self.assertEqual(schema.extract_json('```json\n[1, 2]\n```'), [1, 2])
        with self.assertRaises(ValueError):
            schema.extract_json("nothing")


if __name__ == "__main__":
    unittest.main()
