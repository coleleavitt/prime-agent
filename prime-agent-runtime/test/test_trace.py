from __future__ import annotations

import asyncio
import builtins
import os
import sys
import unittest
from unittest import mock

from rlm import trace

bash_module = sys.modules["rlm.bash"]

TRACE_ID = "4bf92f3577b34da6a3ce929d0e0e4736"
SPAN_ID = "00f067aa0ba902b7"
HEADER = f"00-{TRACE_ID}-{SPAN_ID}-01"


def _is_hex(value: str, length: int) -> bool:
    return len(value) == length and all(c in "0123456789abcdef" for c in value) and set(value) != {"0"}


class ParseFormatTest(unittest.TestCase):
    def test_round_trip(self):
        ctx = trace.parse_traceparent(HEADER)
        self.assertEqual(ctx, trace.TraceContext(trace_id=TRACE_ID, span_id=SPAN_ID, flags="01"))
        self.assertIsNone(ctx.parent_span_id)
        self.assertEqual(trace.format_traceparent(ctx), HEADER)
        self.assertEqual(trace.format_traceparent(trace.parse_traceparent(trace.format_traceparent(ctx))), HEADER)

    def test_flags_are_preserved(self):
        ctx = trace.parse_traceparent(f"00-{TRACE_ID}-{SPAN_ID}-00")
        self.assertEqual(ctx.flags, "00")
        self.assertEqual(trace.format_traceparent(ctx), f"00-{TRACE_ID}-{SPAN_ID}-00")

    def test_rejects_invalid_values(self):
        invalid = [
            None,
            42,
            "",
            "garbage",
            f"01-{TRACE_ID}-{SPAN_ID}-01",  # wrong version
            f"ff-{TRACE_ID}-{SPAN_ID}-01",
            f"00-{TRACE_ID[:-1]}-{SPAN_ID}-01",  # short trace id
            f"00-{TRACE_ID}0-{SPAN_ID}-01",  # long trace id
            f"00-{TRACE_ID}-{SPAN_ID[:-1]}-01",  # short span id
            f"00-{TRACE_ID}-{SPAN_ID}0-01",  # long span id
            f"00-{'0' * 32}-{SPAN_ID}-01",  # all-zero trace id
            f"00-{TRACE_ID}-{'0' * 16}-01",  # all-zero span id
            f"00-{TRACE_ID}-{SPAN_ID}-1",  # short flags
            f"00-{TRACE_ID}-{SPAN_ID}-001",  # long flags
            f"00-{TRACE_ID}-{SPAN_ID}-0g",  # non-hex flags
            f"00-{TRACE_ID.upper()}-{SPAN_ID}-01",  # uppercase is not W3C
            f"00-{TRACE_ID}-{SPAN_ID.upper()}-01",
            f"00-{TRACE_ID}-{SPAN_ID}-01-extra",
            f"00-{TRACE_ID}-{SPAN_ID}",
            f" {HEADER}",
            f"{HEADER}\n",
            f"00_{TRACE_ID}_{SPAN_ID}_01",
        ]
        for value in invalid:
            with self.subTest(value=value):
                self.assertIsNone(trace.parse_traceparent(value))

    def test_ids_are_random_hex_and_never_zero(self):
        seen = set()
        for _ in range(64):
            trace_id = trace.new_trace_id()
            span_id = trace.new_span_id()
            self.assertTrue(_is_hex(trace_id, 32), trace_id)
            self.assertTrue(_is_hex(span_id, 16), span_id)
            seen.add((trace_id, span_id))
        self.assertEqual(len(seen), 64)

    def test_random_id_retries_all_zero_draws(self):
        draws = iter(["0" * 16, "0" * 16, "00000000000000a1"])
        with mock.patch.object(trace.secrets, "token_hex", lambda n: next(draws)):
            self.assertEqual(trace.new_span_id(), "00000000000000a1")


class ContextTest(unittest.TestCase):
    def setUp(self) -> None:
        self.assertIsNone(trace.current())

    def test_set_current_and_reset(self):
        ctx = trace.parse_traceparent(HEADER)
        token = trace.set_current(ctx)
        self.assertIs(trace.current(), ctx)
        trace.reset(token)
        self.assertIsNone(trace.current())

    def test_set_current_none_clears(self):
        outer = trace.set_current(trace.parse_traceparent(HEADER))
        inner = trace.set_current(None)
        self.assertIsNone(trace.current())
        trace.reset(inner)
        self.assertEqual(trace.current().trace_id, TRACE_ID)
        trace.reset(outer)

    def test_start_span_without_context_mints_new_trace(self):
        with trace.start_span("root") as span:
            self.assertIs(trace.current(), span.ctx)
            self.assertTrue(_is_hex(span.trace_id, 32))
            self.assertTrue(_is_hex(span.span_id, 16))
            self.assertIsNone(span.parent_span_id)
            self.assertEqual(span.ctx.flags, "01")
        self.assertIsNone(trace.current())

    def test_start_span_nests_under_current(self):
        parent = trace.parse_traceparent(HEADER)
        token = trace.set_current(parent)
        try:
            with trace.start_span("child") as child:
                self.assertEqual(child.trace_id, TRACE_ID)
                self.assertNotEqual(child.span_id, SPAN_ID)
                self.assertEqual(child.parent_span_id, SPAN_ID)
                with trace.start_span("grandchild") as grandchild:
                    self.assertEqual(grandchild.trace_id, TRACE_ID)
                    self.assertEqual(grandchild.parent_span_id, child.span_id)
                    self.assertNotEqual(grandchild.span_id, child.span_id)
                    self.assertIs(trace.current(), grandchild.ctx)
                self.assertIs(trace.current(), child.ctx)
            self.assertIs(trace.current(), parent)
        finally:
            trace.reset(token)

    def test_child_inherits_flags(self):
        token = trace.set_current(trace.parse_traceparent(f"00-{TRACE_ID}-{SPAN_ID}-00"))
        try:
            with trace.start_span("child") as child:
                self.assertEqual(child.ctx.flags, "00")
        finally:
            trace.reset(token)

    def test_context_restored_on_exception_and_status_error(self):
        events: list[dict] = []
        trace.set_span_emitter(events.append)
        self.addCleanup(trace.set_span_emitter, None)
        with self.assertRaises(ValueError):
            with trace.start_span("boom") as span:
                raise ValueError("bad")
        self.assertIsNone(trace.current())
        self.assertTrue(span.ended)
        self.assertEqual(span.status, "error")
        self.assertEqual(events[-1]["status"], "error")
        self.assertEqual(events[-1]["attrs"]["error"], "ValueError: bad")

    def test_context_isolated_per_task(self):
        async def main():
            async def leaf(name):
                with trace.start_span(name) as span:
                    await asyncio.sleep(0.01)
                    self.assertIs(trace.current(), span.ctx)
                    return span

            with trace.start_span("root") as root:
                a, b = await asyncio.gather(leaf("a"), leaf("b"))
            self.assertEqual({a.parent_span_id, b.parent_span_id}, {root.span_id})
            self.assertNotEqual(a.span_id, b.span_id)
            self.assertIsNone(trace.current())

        asyncio.run(main())

    def test_threads_start_without_context(self):
        import threading

        seen = []
        with trace.start_span("root"):
            thread = threading.Thread(target=lambda: seen.append(trace.current()))
            thread.start()
            thread.join()
        self.assertEqual(seen, [None])


class EmitterTest(unittest.TestCase):
    def test_default_emitter_is_noop(self):
        trace.set_span_emitter(None)
        with trace.start_span("quiet"):
            pass

    def test_span_end_payload_shape(self):
        events: list[dict] = []
        trace.set_span_emitter(events.append)
        self.addCleanup(trace.set_span_emitter, None)
        with trace.start_span("kernel.cell", **{"kernel.request_id": "r1", "n": 2}) as span:
            pass
        self.assertEqual(len(events), 1)
        event = events[0]
        self.assertEqual(
            list(event),
            ["event", "msg", "name", "traceId", "spanId", "durationMs", "status", "attrs"],
        )
        self.assertEqual(event["event"], "trace")
        self.assertEqual(event["msg"], "span_end")
        self.assertEqual(event["name"], "kernel.cell")
        self.assertEqual(event["traceId"], span.trace_id)
        self.assertEqual(event["spanId"], span.span_id)
        self.assertNotIn("parentSpanId", event)
        self.assertIsInstance(event["durationMs"], float)
        self.assertGreaterEqual(event["durationMs"], 0.0)
        self.assertEqual(event["status"], "ok")
        self.assertEqual(event["attrs"], {"kernel.request_id": "r1", "n": 2})

    def test_child_payload_carries_parent_span_id(self):
        events: list[dict] = []
        trace.set_span_emitter(events.append)
        self.addCleanup(trace.set_span_emitter, None)
        with trace.start_span("outer") as outer:
            with trace.start_span("inner") as inner:
                pass
        self.assertEqual([e["name"] for e in events], ["inner", "outer"])
        self.assertEqual(events[0]["parentSpanId"], outer.span_id)
        self.assertEqual(events[0]["spanId"], inner.span_id)
        self.assertEqual(events[0]["traceId"], outer.trace_id)
        self.assertNotIn("parentSpanId", events[1])

    def test_explicit_status_and_end_idempotent(self):
        events: list[dict] = []
        trace.set_span_emitter(events.append)
        self.addCleanup(trace.set_span_emitter, None)
        with trace.start_span("handled") as span:
            span.status = "error"
            span.error = "cell failed"
        self.assertEqual(events[-1]["status"], "error")
        self.assertEqual(events[-1]["attrs"], {"error": "cell failed"})
        span.end()
        span.end(status="ok")
        self.assertEqual(len(events), 1)
        with trace.start_span("manual") as manual:
            manual.end(status="error")
        self.assertEqual(len(events), 2)
        self.assertEqual(events[-1]["status"], "error")

    def test_emitter_failure_never_propagates(self):
        def broken(event):
            raise RuntimeError("sink down")

        trace.set_span_emitter(broken)
        self.addCleanup(trace.set_span_emitter, None)
        with trace.start_span("safe"):
            pass
        self.assertIsNone(trace.current())


class EnvTest(unittest.TestCase):
    def test_inject_env_sets_traceparent_from_current(self):
        with trace.start_span("root") as span:
            env = trace.inject_env({"PATH": "/bin", "TRACEPARENT": "stale"})
            self.assertEqual(env["TRACEPARENT"], trace.format_traceparent(span.ctx))
            self.assertEqual(env["PATH"], "/bin")
            with mock.patch.dict(os.environ, {"HOME_MARK": "1"}):
                default = trace.inject_env()
            self.assertEqual(default["HOME_MARK"], "1")
            self.assertEqual(default["TRACEPARENT"], trace.format_traceparent(span.ctx))

    def test_inject_env_without_context_leaves_env_unchanged(self):
        source = {"PATH": "/bin", "TRACEPARENT": HEADER}
        env = trace.inject_env(source)
        self.assertEqual(env, source)
        self.assertIsNot(env, source)
        self.assertNotIn("TRACEPARENT", trace.inject_env({"PATH": "/bin"}))

    def test_from_env(self):
        self.assertEqual(trace.from_env({"TRACEPARENT": HEADER}), trace.parse_traceparent(HEADER))
        self.assertIsNone(trace.from_env({"TRACEPARENT": "nope"}))
        self.assertIsNone(trace.from_env({}))
        with mock.patch.dict(os.environ, {"TRACEPARENT": HEADER}):
            self.assertEqual(trace.from_env().trace_id, TRACE_ID)
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("TRACEPARENT", None)
            self.assertIsNone(trace.from_env())

    def test_bash_child_env_carries_traceparent(self):
        with trace.start_span("cell") as span:
            env = bash_module._child_env()
        self.assertEqual(env["TRACEPARENT"], trace.format_traceparent(span.ctx))
        self.assertEqual(env["NO_COLOR"], "1")
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("TRACEPARENT", None)
            self.assertNotIn("TRACEPARENT", bash_module._child_env())


class OtelBridgeTest(unittest.TestCase):
    def test_noop_when_opentelemetry_missing(self):
        real_import = builtins.__import__

        def no_otel(name, *args, **kwargs):
            if name == "opentelemetry" or name.startswith("opentelemetry."):
                raise ModuleNotFoundError(name)
            return real_import(name, *args, **kwargs)

        with mock.patch.dict(
            sys.modules, {"opentelemetry": None, "opentelemetry.trace": None, "opentelemetry.context": None}
        ):
            with mock.patch.object(builtins, "__import__", no_otel):
                ctx = trace.parse_traceparent(HEADER)
                self.assertIsNone(trace._otel_attach(ctx))
                trace._otel_detach(None)
                token = trace.set_current(ctx)
                self.assertIsNone(token._otel_token)
                self.assertIs(trace.current(), ctx)
                trace.reset(token)
                with trace.start_span("bridged") as span:
                    self.assertIs(trace.current(), span.ctx)
        self.assertIsNone(trace.current())

    def test_bridge_swallows_attach_failures(self):
        class BrokenTrace:
            def __getattr__(self, name):
                raise RuntimeError("otel exploded")

        fake_pkg = type(sys)("opentelemetry")
        with mock.patch.dict(
            sys.modules,
            {"opentelemetry": fake_pkg, "opentelemetry.trace": BrokenTrace(), "opentelemetry.context": BrokenTrace()},
        ):
            token = trace.set_current(trace.parse_traceparent(HEADER))
            self.assertIsNone(token._otel_token)
            trace.reset(token)
        self.assertIsNone(trace.current())

    def test_bridge_attaches_when_api_present(self):
        # A minimal stand-in for opentelemetry-api: enough to prove the bridge
        # builds a remote NonRecordingSpan and attaches/detaches it.
        calls: list[tuple] = []

        class SpanContext:
            def __init__(self, trace_id, span_id, is_remote, trace_flags):
                calls.append(("span_context", trace_id, span_id, is_remote, int(trace_flags)))

        class NonRecordingSpan:
            def __init__(self, span_context):
                calls.append(("span",))

        otel_trace = type(sys)("opentelemetry.trace")
        otel_trace.SpanContext = SpanContext
        otel_trace.NonRecordingSpan = NonRecordingSpan
        otel_trace.TraceFlags = int
        otel_trace.set_span_in_context = lambda span, context=None: ("ctx", span)
        otel_context = type(sys)("opentelemetry.context")
        otel_context.attach = lambda ctx: calls.append(("attach",)) or "otel-token"
        otel_context.detach = lambda token: calls.append(("detach", token))
        pkg = type(sys)("opentelemetry")
        pkg.trace = otel_trace
        pkg.context = otel_context
        with mock.patch.dict(
            sys.modules,
            {"opentelemetry": pkg, "opentelemetry.trace": otel_trace, "opentelemetry.context": otel_context},
        ):
            token = trace.set_current(trace.parse_traceparent(HEADER))
            self.assertEqual(token._otel_token, "otel-token")
            trace.reset(token)
        self.assertEqual(
            calls,
            [
                ("span_context", int(TRACE_ID, 16), int(SPAN_ID, 16), True, 1),
                ("span",),
                ("attach",),
                ("detach", "otel-token"),
            ],
        )


if __name__ == "__main__":
    unittest.main()
