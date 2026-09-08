"""Runtime spans around bash() commands and MCP calls (see repl.md "Trace context")."""

from __future__ import annotations

import asyncio
import signal
import sys
import threading
import unittest
from types import SimpleNamespace
from unittest import mock

from mcp.types import CallToolResult, TextContent
from rlm import bash, mcp, trace

bash_module = sys.modules["rlm.bash"]


class _SpanCapture(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self.events: list[dict] = []
        trace.set_span_emitter(self.events.append)
        self.addCleanup(trace.set_span_emitter, None)

    def spans(self, name: str) -> list[dict]:
        return [event for event in self.events if event.get("msg") == "span_end" and event["name"] == name]

    def starts(self, name: str) -> list[dict]:
        return [event for event in self.events if event.get("msg") == "span_start" and event["name"] == name]


class BashCommandSpanTest(_SpanCapture):
    async def test_ok_span_nests_under_ambient_context(self):
        with trace.start_span("cell") as cell:
            result = await bash("echo hi")
        self.assertEqual(result.exit_code, 0)
        spans = self.spans("bash.command")
        self.assertEqual(len(spans), 1)
        span = spans[0]
        self.assertEqual(span["status"], "ok")
        self.assertEqual(span["traceId"], cell.trace_id)
        self.assertEqual(span["parentSpanId"], cell.span_id)
        self.assertNotEqual(span["spanId"], cell.span_id)
        attrs = span["attrs"]
        self.assertEqual(attrs["bash.command"], "echo hi")
        self.assertEqual(attrs["bash.exit_code"], 0)
        self.assertIsInstance(attrs["bash.pid"], int)
        self.assertGreaterEqual(attrs["bash.output_bytes"], len("hi\n"))
        self.assertNotIn("bash.signal", attrs)
        self.assertNotIn("bash.killed", attrs)
        self.assertNotIn("error", attrs)
        # The cell span ends after the command span (both emitted once).
        self.assertEqual(
            [(event["msg"], event["name"]) for event in self.events if "name" in event],
            [("span_start", "bash.command"), ("span_end", "bash.command"), ("span_end", "cell")],
        )

    async def test_start_span_and_inventory_expose_progress_without_live_handle(self):
        handle = bash("printf hello; sleep 30")
        self.addCleanup(handle.kill, signal.SIGKILL)
        for _ in range(100):
            inventory = bash_module.active_bash_commands()
            if inventory and inventory[0]["bash.output_bytes"] >= 5:
                break
            await asyncio.sleep(0.02)
        (start,) = self.starts("bash.command")
        record = next(item for item in inventory if item["bash.pid"] == handle.pid)
        self.assertEqual(start["attrs"]["bash.pid"], handle.pid)
        self.assertEqual(start["attrs"]["bash.pgid"], record["bash.pgid"])
        self.assertEqual(start["attrs"]["bash.started_at"], record["bash.started_at"])
        self.assertGreaterEqual(record["bash.output_bytes"], 5)
        self.assertIn("bash.last_output_at", record)
        self.assertNotIn("handle", record)
        record["bash.pid"] = -1
        self.assertEqual(handle.pid, start["attrs"]["bash.pid"])
        handle.kill(sig=signal.SIGKILL)
        await handle

    async def test_cargo_lock_wait_emits_structured_event_and_end_attribute(self):
        command = "printf 'Blocking waiting for file lock on build directory'; sleep 0.1"
        await bash(command)
        event = next(event for event in self.events if event.get("msg") == "cargo_lock_wait")
        self.assertEqual(event["component"], "bash")
        self.assertEqual(event["bash.wait_reason"], "cargo_build_lock")
        self.assertGreaterEqual(event["bash.output_bytes"], 1)
        (span,) = self.spans("bash.command")
        self.assertEqual(span["attrs"]["bash.wait_reason"], "cargo_build_lock")
        self.assertIn("bash.last_output_at", span["attrs"])
        self.assertGreaterEqual(span["attrs"]["bash.elapsed_ms"], 0)
        self.assertGreaterEqual(span["attrs"]["bash.silence_ms"], 0)

    async def test_cargo_lock_detection_survives_output_chunk_boundary(self):
        handle = bash("true")
        await handle
        handle._wait_reason = None
        handle._cargo_probe_tail = b""
        handle._record_output(b"Blocking waiting for file lock on build direc")
        handle._record_output(b"tory")
        self.assertEqual(handle._wait_reason, "cargo_build_lock")

    async def test_no_output_warning_is_structured_and_contains_no_output(self):
        with mock.patch.dict(bash_module.os.environ, {"PRIME_AGENT_BASH_NO_OUTPUT_WARN_MS": "20"}):
            handle = bash("sleep 0.15")
            await handle
        warnings = [event for event in self.events if event.get("msg") == "command_no_output"]
        self.assertTrue(warnings)
        warning = warnings[0]
        self.assertEqual(warning["component"], "bash")
        self.assertEqual(warning["bash.output_bytes"], 0)
        self.assertNotIn("output", warning)
        self.assertGreaterEqual(warning["bash.silence_ms"], 20)

    async def test_command_observability_redacts_secrets(self):
        handle = bash("printf ok # --token=super-secret")
        await handle
        (start,) = self.starts("bash.command")
        (span,) = self.spans("bash.command")
        self.assertNotIn("super-secret", start["attrs"]["bash.command"])
        self.assertEqual(start["attrs"]["bash.command"], span["attrs"]["bash.command"])

    async def test_span_without_ambient_context_starts_a_trace(self):
        self.assertIsNone(trace.current())
        await bash("true")
        (span,) = self.spans("bash.command")
        self.assertNotIn("parentSpanId", span)
        self.assertEqual(len(span["traceId"]), 32)

    async def test_nonzero_exit_is_error_status(self):
        result = await bash("exit 3")
        self.assertEqual(result.exit_code, 3)
        (span,) = self.spans("bash.command")
        self.assertEqual(span["status"], "error")
        self.assertEqual(span["attrs"]["bash.exit_code"], 3)
        self.assertEqual(span["attrs"]["error"], "exit code 3")
        self.assertNotIn("bash.signal", span["attrs"])

    async def test_child_sees_traceparent_of_the_command_span(self):
        with trace.start_span("cell") as cell:
            result = await bash('printf "%s" "$TRACEPARENT"')
        (span,) = self.spans("bash.command")
        ctx = trace.parse_traceparent(result.output.strip())
        self.assertIsNotNone(ctx, result.output)
        self.assertEqual(ctx.span_id, span["spanId"])
        self.assertEqual(ctx.trace_id, cell.trace_id)
        self.assertNotEqual(ctx.span_id, cell.span_id)

    async def test_command_attribute_is_truncated(self):
        command = "true # " + "x" * 400
        await bash(command)
        (span,) = self.spans("bash.command")
        recorded = span["attrs"]["bash.command"]
        self.assertEqual(len(recorded), 200)
        self.assertTrue(recorded.endswith("..."))
        self.assertTrue(command.startswith(recorded[:-3]))

    async def test_killed_process_ends_span_once_with_signal(self):
        handle = bash("trap '' TERM; echo up; sleep 30")
        for _ in range(100):
            if "up" in handle.output():
                break
            await asyncio.sleep(0.05)
        handle.kill(grace=0.2)
        result = await asyncio.wait_for(handle, timeout=10)
        self.assertEqual(result.exit_code, -9)
        # Let the watcher thread reap the group; the span must still be emitted only once.
        for _ in range(100):
            if not handle.running:
                break
            await asyncio.sleep(0.05)
        (span,) = self.spans("bash.command")
        self.assertEqual(span["status"], "error")
        attrs = span["attrs"]
        self.assertEqual(attrs["bash.exit_code"], -9)
        self.assertEqual(attrs["bash.signal"], "SIGKILL")
        self.assertTrue(attrs["bash.killed"])
        self.assertEqual(attrs["error"], "killed by SIGKILL")

    async def test_terminated_process_records_sigterm(self):
        handle = bash("sleep 30")
        handle.kill(grace=5.0)
        result = await asyncio.wait_for(handle, timeout=10)
        self.assertNotEqual(result.exit_code, 0)
        (span,) = self.spans("bash.command")
        self.assertEqual(span["status"], "error")
        self.assertTrue(span["attrs"]["bash.killed"])
        if result.exit_code < 0:
            self.assertEqual(span["attrs"]["bash.signal"], "SIGTERM")

    async def test_poll_only_observer_still_gets_one_span(self):
        handle = bash("echo polled")
        for _ in range(200):
            if handle.poll() is not None:
                break
            await asyncio.sleep(0.02)
        self.assertIsNotNone(handle.poll())
        (span,) = self.spans("bash.command")
        self.assertEqual(span["attrs"]["bash.exit_code"], 0)

    async def test_kernel_shutdown_ends_running_span_once(self):
        handle = bash("sleep 30")
        self.assertTrue(handle.running)
        bash_module._kill_live_handles()
        result = await asyncio.wait_for(handle, timeout=10)
        self.assertNotEqual(result.exit_code, 0)
        for _ in range(100):
            if not handle.running:
                break
            await asyncio.sleep(0.05)
        (span,) = self.spans("bash.command")
        self.assertEqual(span["status"], "error")
        self.assertEqual(span["attrs"]["error"], "kernel shutdown")
        self.assertTrue(span["attrs"]["bash.killed"])
        self.assertNotIn("bash.exit_code", span["attrs"])
        self.assertTrue(handle._span.ended)

    async def test_spawn_failure_ends_span_with_error(self):
        with mock.patch.object(bash_module.subprocess, "Popen", side_effect=OSError("no fork")):
            with self.assertRaises(OSError):
                bash("echo never")
        (span,) = self.spans("bash.command")
        self.assertEqual(span["status"], "error")
        self.assertEqual(span["attrs"]["error"], "spawn failed: OSError: no fork")
        self.assertNotIn("bash.pid", span["attrs"])

    async def test_concurrent_worker_threads_end_span_exactly_once(self):
        handle = bash("sleep 30")
        barrier = threading.Barrier(3)
        threads = [
            threading.Thread(target=lambda: (barrier.wait(), handle._end_span(error="race")))
            for _ in range(2)
        ]
        for thread in threads:
            thread.start()
        barrier.wait()
        for thread in threads:
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())
        handle.kill(signal.SIGKILL)
        await asyncio.sleep(0.1)
        self.assertEqual(len(self.spans("bash.command")), 1)

    async def test_end_span_never_raises(self):
        handle = bash("true")
        await handle
        handle._buffer = None  # a broken handle must not turn tracing into an exception
        handle._span.ended = False
        handle._end_span(exit_code=0)
        self.assertEqual(len(self.spans("bash.command")), 1)


class FakeSession:
    def __init__(self, tools, result=None, error: BaseException | None = None):
        self.tools = tools
        self.result = result
        self.error = error
        self.calls: list[tuple[str, dict]] = []

    async def list_tools(self):
        return SimpleNamespace(tools=self.tools)

    async def call_tool(self, name, arguments):
        self.calls.append((name, arguments))
        if self.error is not None:
            raise self.error
        return self.result


class McpCallSpanTest(_SpanCapture):
    def setUp(self) -> None:
        super().setUp()
        mcp._registry = mcp._Registry()

    async def generation(self, result=None, error=None) -> mcp._Generation:
        generation = mcp._Generation("svc", {"type": "http"})
        tools = [SimpleNamespace(name="echo", description="", inputSchema={"type": "object"})]
        generation.session = FakeSession(tools, result=result, error=error)
        await generation.discover()
        return generation

    async def test_call_tool_ok(self):
        generation = await self.generation(result=CallToolResult(content=[TextContent(type="text", text="pong")]))
        with mock.patch.object(mcp._registry, "_get_locked", mock.AsyncMock(return_value=generation)):
            with trace.start_span("cell") as cell:
                value = await mcp.call_tool("svc", "echo", {"value": 1})
        self.assertEqual(value, "pong")
        (span,) = self.spans("mcp.call")
        self.assertEqual(span["status"], "ok")
        self.assertEqual(span["traceId"], cell.trace_id)
        self.assertEqual(span["parentSpanId"], cell.span_id)
        self.assertEqual(
            span["attrs"], {"mcp.server": "svc", "mcp.tool": "echo", "mcp.connected": False}
        )
        self.assertEqual(generation.session.calls, [("echo", {"value": 1})])

    async def test_connected_attribute_reflects_open_generation(self):
        generation = await self.generation(result=CallToolResult(content=[TextContent(type="text", text="pong")]))
        mcp._registry._generations["svc"] = generation
        with mock.patch.object(mcp._registry, "_get_locked", mock.AsyncMock(return_value=generation)):
            await mcp.call_tool("svc", "echo")
        (span,) = self.spans("mcp.call")
        self.assertTrue(span["attrs"]["mcp.connected"])

    async def test_call_tool_error_propagates_unchanged(self):
        generation = await self.generation(error=RuntimeError("tool exploded"))
        with mock.patch.object(mcp._registry, "_get_locked", mock.AsyncMock(return_value=generation)):
            with self.assertRaisesRegex(RuntimeError, "tool exploded"):
                await mcp.call_tool("svc", "echo", {})
        (span,) = self.spans("mcp.call")
        self.assertEqual(span["status"], "error")
        self.assertEqual(span["attrs"]["mcp.tool"], "echo")
        self.assertEqual(span["attrs"]["error"], "RuntimeError: tool exploded")
        self.assertIsNone(trace.current())

    async def test_invalid_tool_name_is_recorded_as_error(self):
        with self.assertRaises(TypeError):
            await mcp.call_tool("svc", "")
        with self.assertRaises(TypeError):
            await mcp.call_tool("svc", 42)  # type: ignore[arg-type]
        spans = self.spans("mcp.call")
        self.assertEqual([span["status"] for span in spans], ["error", "error"])
        self.assertEqual(spans[0]["attrs"]["mcp.tool"], "")
        self.assertEqual(spans[1]["attrs"]["mcp.tool"], "42")

    async def test_list_tools_span(self):
        generation = await self.generation()
        with mock.patch.object(mcp._registry, "_get_locked", mock.AsyncMock(return_value=generation)):
            tools = await mcp.list_tools("svc")
        self.assertEqual([tool["name"] for tool in tools], ["echo"])
        (span,) = self.spans("mcp.call")
        self.assertEqual(span["status"], "ok")
        self.assertEqual(span["attrs"]["mcp.tool"], "list_tools")
        self.assertEqual(span["attrs"]["mcp.server"], "svc")
        self.assertEqual(span["attrs"]["mcp.tool_count"], 1)

    async def test_list_tools_error(self):
        async def unavailable(_server):
            raise RuntimeError("host request timed out")

        with mock.patch.object(mcp, "_config", unavailable):
            with self.assertRaises(RuntimeError):
                await mcp.list_tools("svc")
        (span,) = self.spans("mcp.call")
        self.assertEqual(span["status"], "error")
        self.assertIn("host request timed out", span["attrs"]["error"])


if __name__ == "__main__":
    unittest.main()
