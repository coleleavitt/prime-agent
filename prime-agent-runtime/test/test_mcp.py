from __future__ import annotations

import asyncio
import traceback
import unittest
from unittest import mock

from rlm import McpToolError, mcp, repl


def run(coro):
    return asyncio.run(coro)


def ok(value, connected=False):
    return {"status": "ok", "result": {"ok": True, "value": value, "connected": connected}}


def failed(kind, message, connected=False):
    return {
        "status": "ok",
        "result": {"ok": False, "error": {"type": kind, "message": message}, "connected": connected},
    }


class FakeHost:
    """Stands in for the host's ``mcp.session.*`` handlers behind
    ``repl.host_request``: ``answer(data)`` returns the raw reply envelope."""

    def __init__(self, answer):
        self.answer = answer
        self.requests: list[dict] = []
        self.options: list[dict] = []

    async def host_request(self, data, **options):
        self.requests.append(dict(data))
        self.options.append(options)
        reply = self.answer(data)
        if asyncio.iscoroutine(reply):
            reply = await reply
        return reply

    def __enter__(self):
        self._patch = mock.patch.object(repl, "host_request", self.host_request)
        self._patch.start()
        return self

    def __exit__(self, *_exc):
        self._patch.stop()


class McpRegistryTest(unittest.TestCase):
    def setUp(self):
        mcp._client = mcp._Client()

    def test_status_reports_tools_and_errors_per_server(self):
        async def ok_listing(server):
            return [{"name": f"{server}.tool", "description": "fixture description", "inputSchema": {}}]

        with mock.patch.object(mcp, "list_tools", ok_listing):
            result = run(mcp.status(["alpha", "beta"], 60_000.0))
        self.assertEqual(
            result,
            [
                {"server": "alpha", "tools": [{"name": "alpha.tool", "description": "fixture description"}], "error": None},
                {"server": "beta", "tools": [{"name": "beta.tool", "description": "fixture description"}], "error": None},
            ],
        )

    def test_status_isolates_failures_and_timeouts(self):
        async def failing_listing(server):
            raise RuntimeError(f"no config for {server}")

        async def slow_listing(server):
            await asyncio.sleep(1.0)
            return []

        with mock.patch.object(mcp, "list_tools", failing_listing):
            result = run(mcp.status(["broken"], 60_000.0))
        self.assertIsNone(result[0]["tools"])
        self.assertEqual(result[0]["error"], "RuntimeError: no config for broken")

        with mock.patch.object(mcp, "list_tools", slow_listing):
            result = run(mcp.status(["slow"], 50.0))
        self.assertIsNone(result[0]["tools"])
        self.assertIn("TimeoutError", result[0]["error"])

    def test_diagnostics_do_not_contain_headers_or_env_secrets(self):
        async def host_request(*_args):
            raise RuntimeError("bridge failed")

        with mock.patch.object(mcp, "host_request", host_request):
            with self.assertRaises(RuntimeError) as caught:
                run(mcp.call_tool("svc", "tool", {"secret": "do-not-print"}))
        self.assertNotIn("do-not-print", str(caught.exception))

    def test_call_tool_is_one_cancellable_host_request(self):
        with FakeHost(lambda data: ok({"value": "ok"})) as host:
            value = run(mcp.call_tool("svc", "http/raw.tool", {"value": "ok"}))
        self.assertEqual(value, {"value": "ok"})
        self.assertEqual(
            host.requests,
            [{"type": "mcp.session.call_tool", "server": "svc", "tool": "http/raw.tool", "arguments": {"value": "ok"}}],
        )
        self.assertEqual(host.options, [{"cancel_on_cancel": True, "drain_timeout_ms": mcp._CANCEL_DRAIN_MS}])

    def test_host_errors_raise_the_class_and_message_the_host_names(self):
        unavailable = (
            "MCP credentials for 'github' are not available. Ask the user to connect it "
            "(/plugins or /mcp login github); do not ask them to set environment variables."
        )
        cases = [
            ("PermissionError", "MCP tool 'denied' is disabled for server 'svc'", PermissionError),
            ("KeyError", "MCP server 'svc' has no tool 'missing'", KeyError),
            ("McpCredentialsUnavailable", unavailable, mcp.McpCredentialsUnavailable),
            ("McpStartupError", "MCP stdio server failed during startup (MCPError: Connection closed).", mcp.McpStartupError),
            ("McpDiscoveryError", "MCP server 'svc' repeated a tools/list pagination cursor", mcp.McpDiscoveryError),
            ("McpToolError", "redacted failure", McpToolError),
            ("ValueError", "MCP timeouts must be positive milliseconds", ValueError),
            ("FileNotFoundError", "[Errno 2] No such file or directory: 'missing-mcp'", FileNotFoundError),
            ("RuntimeError", "MCP server 'svc' is disabled", RuntimeError),
            ("SomethingNew", "an unmapped failure", RuntimeError),
        ]
        for kind, message, expected in cases:
            with self.subTest(kind=kind):
                with FakeHost(lambda data, kind=kind, message=message: failed(kind, message)):
                    with self.assertRaises(expected) as caught:
                        run(mcp.call_tool("svc", "tool"))
                self.assertIs(type(caught.exception), expected)
                self.assertEqual(caught.exception.args, (message,))
        with FakeHost(lambda data: failed("TimeoutError", "")):
            with self.assertRaises(TimeoutError) as caught:
                run(mcp.call_tool("svc", "tool"))
        self.assertEqual(str(caught.exception), "")
        # The static-token failure keeps the kernel's own unavailable error.
        self.assertTrue(issubclass(mcp.McpCredentialsUnavailable, RuntimeError))

    def test_a_failed_or_malformed_host_reply_is_a_runtime_error(self):
        for reply in ({"status": "error", "error": "host request type unavailable"}, {"status": "ok", "result": []}):
            with self.subTest(reply=reply):
                with FakeHost(lambda data, reply=reply: reply):
                    with self.assertRaises(RuntimeError):
                        run(mcp.list_tools("svc"))

    def test_cancelling_a_call_cancels_the_host_request(self):
        started = asyncio.Event()
        cancelled = asyncio.Event()

        async def answer(data):
            started.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                cancelled.set()
                raise

        async def scenario():
            with FakeHost(answer):
                call = asyncio.create_task(mcp.call_tool("svc", "slow"))
                await started.wait()
                call.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await call
                await cancelled.wait()

        run(scenario())

    def test_a_host_cancelled_reply_raises_cancelled_error(self):
        with FakeHost(lambda data: failed("CancelledError", "")):
            with self.assertRaises(asyncio.CancelledError):
                run(mcp.call_tool("svc", "tool"))

    def test_names_are_validated_before_any_host_request(self):
        with FakeHost(lambda data: ok(None)) as host:
            for call in (lambda: mcp.call_tool("", "tool"), lambda: mcp.call_tool("svc", ""), lambda: mcp.list_tools(42)):
                with self.assertRaises(TypeError):
                    run(call())
            with self.assertRaises(TypeError):
                run(mcp.call_tool("svc", "tool", ["not", "a", "dict"]))
        self.assertEqual(host.requests, [])

    def test_reload_remains_reusable_but_close_is_terminal(self):
        with FakeHost(lambda data: ok(None)) as host:
            run(mcp.reload("svc"))
            run(mcp.reload())
            run(mcp.close())
            run(mcp.close())
            with self.assertRaisesRegex(RuntimeError, "MCP registry is shut down"):
                run(mcp.call_tool("svc", "tool"))
            with self.assertRaisesRegex(RuntimeError, "MCP registry is shut down"):
                run(mcp.reload("svc"))
        self.assertEqual(
            host.requests,
            [
                {"type": "mcp.session.reload", "server": "svc"},
                {"type": "mcp.session.reload"},
                {"type": "mcp.session.close"},
            ],
        )


class McpDiscoveryInventoryTest(unittest.TestCase):
    """The host-backed inventory surface and live tool discovery."""

    def setUp(self):
        mcp._client = mcp._Client()

    # -- inventory pass-through --------------------------------------------

    def _patch_host(self, responses):
        async def host_request(request_type, payload):
            reply = responses[request_type]
            if isinstance(reply, Exception):
                raise reply
            return reply

        return mock.patch.object(mcp, "host_request", host_request)

    def test_list_connections_passes_through_and_scrubs_secret_keys(self):
        responses = {
            "mcp.list_connections": {
                "connections": [
                    {
                        "connectionId": "notion",
                        "label": "Notion",
                        "status": "connected",
                        "accessToken": "tok",
                        "oauth": {"clientSecret": "cs", "kind": "oauth"},
                    },
                    {"connectionId": "acme", "status": "error", "setupHint": "configure API key"},
                ]
            }
        }
        with self._patch_host(responses):
            connections = run(mcp.list_connections())
        self.assertEqual([entry["connectionId"] for entry in connections], ["notion", "acme"])
        notion = connections[0]
        self.assertEqual(notion["label"], "Notion")
        self.assertNotIn("accessToken", notion)
        self.assertEqual(notion["oauth"], {"kind": "oauth"})
        self.assertEqual(connections[1]["setupHint"], "configure API key")

    def test_list_plugins_keeps_the_paste_token_marker_and_still_strips_secrets(self):
        # `pasteToken: true` is the host's boolean marker for rows the user
        # connects by pasting a token (#2678); its key contains "token", so the
        # name heuristic ate it and paste-token logins could never be offered.
        # Only the boolean marker survives: a string under the same key, and
        # every real secret-named key, is still dropped.
        plugins = [
            {
                "serviceId": "acme",
                "pasteToken": True,
                "accessToken": "tok",
                "oauth": {"clientSecret": "cs", "pasteToken": True},
            },
            {"serviceId": "leaky", "pasteToken": "sk-live-secret", "status": "not_connected"},
        ]
        with self._patch_host({"mcp.list_plugins": {"plugins": plugins, "nextCursor": None}}):
            page = run(mcp.list_plugins())
        self.assertEqual(
            page["plugins"],
            [
                {"serviceId": "acme", "pasteToken": True, "oauth": {"pasteToken": True}},
                {"serviceId": "leaky", "status": "not_connected"},
            ],
        )

    def test_list_connections_rejects_malformed_host_data(self):
        # One table: each malformed host reply must fail the whole call instead
        # of passing a broken inventory shape through to the agent.
        for reply in ({"connections": [{"label": "no-connection-id"}]}, {"connections": ["not-a-dict"]}, {"connections": "no"}, ["not", "a", "dict"]):
            with self.subTest(reply=reply):
                with self._patch_host({"mcp.list_connections": reply}):
                    with self.assertRaises(RuntimeError):
                        run(mcp.list_connections())

    def test_inventory_wraps_host_failures_without_echoing_them(self):
        with self._patch_host({"mcp.list_connections": RuntimeError("bridge is down")}):
            with self.assertRaises(RuntimeError) as caught:
                run(mcp.list_connections())
        error = caught.exception
        self.assertEqual(str(error), "MCP mcp.list_connections request failed")
        self.assertIsNone(error.__cause__)
        self.assertIsNone(error.__context__)
        formatted = "".join(traceback.format_exception(type(error), error, error.__traceback__))
        self.assertNotIn("bridge is down", formatted)

    def test_list_plugins_forwards_filters_limit_and_cursor(self):
        captured = {}

        async def host_request(request_type, payload):
            captured["type"] = request_type
            captured["payload"] = payload
            return {
                "plugins": [{"serviceId": "notion", "label": "Notion", "connectionStatus": "not_connected"}],
                "nextCursor": "page-2",
            }

        with mock.patch.object(mcp, "host_request", host_request):
            page = run(mcp.list_plugins(connection_status="not_connected", limit=5, cursor="page-1"))
        self.assertEqual(captured["type"], "mcp.list_plugins")
        self.assertEqual(
            captured["payload"], {"connectionStatus": "not_connected", "limit": 5, "cursor": "page-1"}
        )
        self.assertEqual(page["nextCursor"], "page-2")
        self.assertEqual(page["plugins"][0]["serviceId"], "notion")

        with mock.patch.object(mcp, "host_request", host_request):
            run(mcp.list_plugins())
        self.assertEqual(captured["payload"], {"limit": 50})

    def test_list_plugins_validates_inputs_and_host_shapes(self):
        for kwargs in ({"connection_status": "maybe"}, {"limit": 0}, {"limit": 201}, {"limit": True}, {"cursor": "x" * 600}):
            with self._patch_host({"mcp.list_plugins": {"plugins": []}}):
                with self.assertRaises((ValueError, TypeError)):
                    run(mcp.list_plugins(**kwargs))
        for reply in ({"plugins": ["no"]}, {"plugins": [], "nextCursor": ""}):
            with self._patch_host({"mcp.list_plugins": reply}):
                with self.assertRaises(RuntimeError):
                    run(mcp.list_plugins())

    def test_search_plugins_sends_query_and_limit(self):
        captured = {}

        async def host_request(request_type, payload):
            captured["type"] = request_type
            captured["payload"] = payload
            return {"plugins": [{"serviceId": "notion", "apiKey": "leak"}], "nextCursor": None}

        with mock.patch.object(mcp, "host_request", host_request):
            page = run(mcp.search_plugins("  Notion  "))
        self.assertEqual(captured["type"], "mcp.search_plugins")
        self.assertEqual(captured["payload"], {"query": "Notion", "limit": 10})
        self.assertIsNone(page["nextCursor"])
        self.assertNotIn("apiKey", page["plugins"][0])
        for bad in ("", "   "):
            with self._patch_host({"mcp.search_plugins": {"plugins": []}}):
                with self.assertRaises(TypeError):
                    run(mcp.search_plugins(bad))
        with self._patch_host({"mcp.search_plugins": {"plugins": []}}):
            with self.assertRaises(ValueError):
                run(mcp.search_plugins("notion", limit=51))

    # -- live tool discovery ------------------------------------------------

    # -- live tool discovery ------------------------------------------------

    def test_describe_and_list_tools_return_the_host_inventory(self):
        schema = {"type": "object", "properties": {"query": {"type": "string"}}}
        tool = {"name": "search-docs", "description": "Search docs", "inputSchema": schema}

        def answer(data):
            if data["type"] == "mcp.session.describe_tool":
                return ok(dict(tool))
            return ok([dict(tool)])

        with FakeHost(answer) as host:
            described = run(mcp.describe_tool("notion-work", "search-docs"))
            listed = run(mcp.list_tools("notion-work"))
        self.assertEqual((described, listed), (tool, [tool]))
        self.assertEqual(
            host.requests,
            [
                {"type": "mcp.session.describe_tool", "server": "notion-work", "tool": "search-docs"},
                {"type": "mcp.session.list_tools", "server": "notion-work"},
            ],
        )

    def test_search_tools_scoped_connection_reports_scope_and_truncation(self):
        match = {"connectionId": "notion-work", "name": "search-docs", "description": "Search workspace documents"}

        def answer(data):
            return ok([match][: data["limit"]])

        with FakeHost(answer) as host:
            found = run(mcp.search_tools("  DOCUMENTS ", connection_id="notion-work"))
            limited = run(mcp.search_tools("doc", connection_id="notion-work", limit=1))
        self.assertEqual(found, {"tools": [match], "searched": ["notion-work"], "unavailable": [], "truncated": False})
        self.assertTrue(limited["truncated"])
        self.assertEqual(
            host.requests[0],
            {"type": "mcp.session.search_tools", "server": "notion-work", "query": "DOCUMENTS", "limit": 20},
        )
        # A failing connection surfaces its error instead of hiding it.
        with FakeHost(lambda data: failed("KeyError", "MCP server 'notion-work' is not declared in user settings")):
            with self.assertRaises(KeyError):
                run(mcp.search_tools("documents", connection_id="notion-work"))

    def test_search_tools_without_connection_searches_connected_only(self):
        responses = {
            "mcp.list_connections": {
                "connections": [
                    {"connectionId": "a", "status": "connected"},
                    {"connectionId": "b", "status": "connected"},
                    {"connectionId": "c", "status": "not_connected"},
                    {"connectionId": "d", "status": "error"},
                ]
            }
        }
        match = {"connectionId": "a", "name": "search-docs", "description": "Search workspace documents"}

        def answer(data):
            if data["server"] == "a":
                return ok([match])
            return failed(
                "McpCredentialsUnavailable",
                "MCP credentials for 'b' are not available. Ask the user to connect it"
                " (/plugins or /mcp login b); do not ask them to set environment variables.",
            )

        with self._patch_host(responses), FakeHost(answer) as host:
            result = run(mcp.search_tools("documents"))
        self.assertEqual(
            result,
            {
                "tools": [match],
                "searched": ["a"],
                "unavailable": [
                    {
                        "connectionId": "b",
                        "error": "McpCredentialsUnavailable: credentials for this connection are not available; "
                        "the user must connect it",
                    }
                ],
                "truncated": False,
            },
        )
        self.assertEqual([request["server"] for request in host.requests], ["a", "b"])

    def test_search_tools_bounds_servers_and_reports_truncation(self):
        connections = {"connections": [{"connectionId": f"svc-{index}", "status": "connected"} for index in range(10)]}
        with self._patch_host({"mcp.list_connections": connections}), FakeHost(lambda data: ok([])) as host:
            result = run(mcp.search_tools("anything"))
        self.assertEqual(len(host.requests), mcp._MAX_TOOL_SEARCH_SERVERS)
        self.assertEqual(result["searched"], [f"svc-{index}" for index in range(mcp._MAX_TOOL_SEARCH_SERVERS)])
        self.assertTrue(result["truncated"])

    def test_search_unavailable_never_echoes_raw_exception_text(self):
        connections = {"connections": [{"connectionId": "leaky", "status": "connected"}]}
        raw = "Connection failed: https://user:hunter2@evil.test/mcp?api_key=abc123 Authorization: Bearer tok-123-secret"
        with self._patch_host({"mcp.list_connections": connections}), FakeHost(lambda data: failed("RuntimeError", raw)):
            result = run(mcp.search_tools("documents"))
        error = result["unavailable"][0]["error"]
        self.assertEqual(error, "RuntimeError: the connection could not be opened or searched")
        for leaked in ("hunter2", "abc123", "tok-123-secret", "evil.test", "Bearer"):
            self.assertNotIn(leaked, error)

    def test_host_inventory_failures_never_expose_the_original_exception(self):
        raw = (
            "GET https://user:hunter2@sync.test/mcp?token=tok-123-secret failed; "
            "Authorization: Bearer tok-123-secret; body password=hunter2"
        )

        async def host_request(request_type, payload):
            raise RuntimeError(raw)

        with mock.patch.object(mcp, "host_request", host_request):
            with self.assertRaises(RuntimeError) as caught:
                run(mcp.list_connections())
        error = caught.exception
        self.assertEqual(str(error), "MCP mcp.list_connections request failed")
        self.assertIsNone(error.__cause__)
        self.assertIsNone(error.__context__)
        # The full formatted chain (not just str(exc)) must stay secret-free.
        formatted = "".join(traceback.format_exception(type(error), error, error.__traceback__))
        for leaked in ("hunter2", "tok-123-secret", "sync.test", "Authorization", "Bearer", "password"):
            self.assertNotIn(leaked, formatted)

    def test_host_inventory_timeouts_report_a_fixed_message(self):
        async def hanging_host_request(request_type, payload):
            # The host never answers: the inventory's own timeout bound (the
            # behavior under test) is what settles the call.
            await asyncio.Event().wait()

        with mock.patch.object(mcp, "host_request", hanging_host_request), mock.patch.object(
            mcp, "_INVENTORY_TIMEOUT", 0.01
        ):
            with self.assertRaises(RuntimeError) as caught:
                run(mcp.list_connections())
        self.assertEqual(str(caught.exception), "MCP mcp.list_connections request timed out")

    # -- tools/list pagination ----------------------------------------------

if __name__ == "__main__":
    unittest.main()
