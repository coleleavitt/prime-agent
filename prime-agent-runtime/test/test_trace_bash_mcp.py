"""Runtime spans around bash() commands and MCP calls (see repl.md "Trace context")."""

from __future__ import annotations

import asyncio
import sys
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
        return [event for event in self.events if event["name"] == name]


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
        self.assertEqual([event["name"] for event in self.events], ["bash.command", "cell"])

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
