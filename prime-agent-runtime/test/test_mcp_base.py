from __future__ import annotations

import asyncio
import json
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from rlm import mcp_base, repl
from rlm.mcp_base import McpIntegration, McpToolError, NotEnabled


def _run(coro):
    return asyncio.run(coro)


class _FakeHost:
    """The host's ``mcp.integration.*`` handlers behind ``repl.host_request``:
    a canned tool list and call result (raw reply ``result`` objects)."""

    def __init__(self, tools=(), call=None):
        self.tools = [
            {"name": name, "description": description, "inputSchema": schema}
            for name, description, schema in tools
        ]
        self.call = call if call is not None else {"ok": True, "value": None, "connected": False}
        self.requests = []

    async def host_request(self, data, **_options):
        self.requests.append(dict(data))
        if data["type"] == "mcp.integration.list_tools":
            return {"status": "ok", "result": {"ok": True, "value": self.tools, "connected": False}}
        return {"status": "ok", "result": self.call}

    def patch(self):
        return mock.patch.object(repl, "host_request", self.host_request)


class _Integration(McpIntegration):
    server = "demo"
    url = "https://example.test/mcp"


class McpIntegrationTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.agent_dir = Path(self._tmp.name)
        self.auth_path = self.agent_dir / "auth.json"
        patcher = mock.patch.object(mcp_base, "_agent_dir", return_value=self.agent_dir)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.addCleanup(self._tmp.cleanup)

    def _write_auth(self, cred):
        self.auth_path.write_text(json.dumps({"mcp:demo": cred}))

    def test_not_enabled_without_credentials(self):
        integration = _Integration()
        with self.assertRaises(NotEnabled):
            _run(integration._resolve_token())

    def test_reads_oauth_access_token(self):
        self._write_auth(
            {"type": "oauth", "access": "tok-123", "refresh": "r", "expires": (time.time() + 3600) * 1000}
        )
        self.assertEqual(_run(_Integration()._resolve_token()), "tok-123")

    def test_reads_api_key(self):
        self._write_auth({"type": "api_key", "key": "key-abc"})
        self.assertEqual(_run(_Integration()._resolve_token()), "key-abc")

    def test_api_key_env_indirection_resolved(self):
        self._write_auth({"type": "api_key", "key": "MY_MCP_KEY"})
        with mock.patch.dict("os.environ", {"MY_MCP_KEY": "resolved-secret"}):
            self.assertEqual(_run(_Integration()._resolve_token()), "resolved-secret")

    def test_refreshes_via_host_when_expired(self):
        self._write_auth(
            {"type": "oauth", "access": "old", "refresh": "r", "expires": (time.time() - 10) * 1000}
        )

        async def fake_host_request(req_type, payload):
            self.assertEqual(req_type, "mcp.refresh")
            self.assertEqual(payload, {"server": "demo"})
            # Simulate the host rewriting auth.json with a fresh token.
            self._write_auth(
                {"type": "oauth", "access": "new", "refresh": "r", "expires": (time.time() + 3600) * 1000}
            )
            return {}

        with mock.patch.object(mcp_base, "host_request", fake_host_request):
            self.assertEqual(_run(_Integration()._resolve_token()), "new")

    def test_not_enabled_when_refresh_leaves_token_expired(self):
        # Host refresh "succeeds" but auth.json still holds an expired token →
        # must raise NotEnabled, not return the stale access value.
        self._write_auth(
            {"type": "oauth", "access": "stale", "refresh": "r", "expires": (time.time() - 10) * 1000}
        )

        async def fake_host_request(req_type, payload):
            return {}  # no-op: token stays expired

        with mock.patch.object(mcp_base, "host_request", fake_host_request):
            with self.assertRaises(NotEnabled):
                _run(_Integration()._resolve_token())

    def test_refresh_failure_surfaces_as_error_not_not_enabled(self):
        # Creds exist but the host refresh fails transiently → surface a refresh
        # error, not a misleading NotEnabled (which implies re-login).
        self._write_auth(
            {"type": "oauth", "access": "stale", "refresh": "r", "expires": (time.time() - 10) * 1000}
        )

        async def failing_host_request(req_type, payload):
            raise RuntimeError("network down")

        with mock.patch.object(mcp_base, "host_request", failing_host_request):
            with self.assertRaises(RuntimeError) as ctx:
                _run(_Integration()._resolve_token())
        self.assertNotIsInstance(ctx.exception, NotEnabled)
        self.assertIn("refresh", str(ctx.exception).lower())

    def test_bearer_token_env_wins(self):
        class EnvIntegration(_Integration):
            bearer_token_env = "DEMO_MCP_TOKEN"

        with mock.patch.dict("os.environ", {"DEMO_MCP_TOKEN": "env-secret"}):
            self.assertEqual(_run(EnvIntegration()._resolve_token()), "env-secret")

    def test_auto_bound_tool_calls_session(self):
        host = _FakeHost(
            tools=[("list_issues", "List issues", {"type": "object"})],
            call={"ok": True, "value": {"issues": [1, 2]}, "connected": False},
        )
        self._write_auth(
            {"type": "oauth", "access": "t", "refresh": "r", "expires": (time.time() + 3600) * 1000}
        )
        with host.patch():
            integration = _Integration()
            out = _run(integration.list_issues(team="Eng"))
        self.assertEqual(out, {"issues": [1, 2]})
        connection = {"server": "demo", "url": "https://example.test/mcp", "headers": {"Authorization": "Bearer t"}}
        self.assertEqual(
            host.requests,
            [
                {"type": "mcp.integration.list_tools", **connection},
                {"type": "mcp.integration.call_tool", **connection, "tool": "list_issues", "arguments": {"team": "Eng"}},
            ],
        )

    def test_error_result_raises(self):
        host = _FakeHost(call={"ok": False, "error": {"type": "McpToolError", "message": "boom"}, "connected": False})
        self._write_auth({"type": "api_key", "key": "key-abc"})
        with host.patch():
            with self.assertRaises(McpToolError) as ctx:
                _run(_Integration().call_tool("noop", {}))
        self.assertIn("boom", str(ctx.exception))

    def test_configured_headers_precede_the_bearer_header(self):
        class HeaderIntegration(_Integration):
            async def _resolve_config(self):
                return self.url, {"X-Team": "eng", "Authorization": "Bearer configured"}

        host = _FakeHost()
        self._write_auth(
            {"type": "oauth", "access": "tok-xyz", "refresh": "r", "expires": (time.time() + 3600) * 1000}
        )
        with host.patch():
            _run(HeaderIntegration().call_tool("noop", {}))
        self.assertEqual(host.requests[0]["headers"], {"X-Team": "eng", "Authorization": "Bearer tok-xyz"})

    def test_unknown_tool_raises_with_available_list(self):
        host = _FakeHost(tools=[("list_issues", "", {})])
        self._write_auth(
            {"type": "oauth", "access": "t", "refresh": "r", "expires": (time.time() + 3600) * 1000}
        )
        with host.patch():
            integration = _Integration()
            with self.assertRaises(AttributeError) as ctx:
                _run(integration.nonexistent_tool())
        self.assertIn("list_issues", str(ctx.exception))

    def test_requires_server_attribute(self):
        class Bad(McpIntegration):
            server = ""

        with self.assertRaises(ValueError):
            Bad()

    def test_resolve_config_ignores_host_overrides(self):
        # A same-named mcpServers entry must not repoint an authored integration:
        # its credentials (auth.json or a bearer-token env var) would follow.
        async def host_with_override(req_type, payload):
            raise AssertionError("authored integrations must not consult mcp.config")

        with mock.patch.object(mcp_base, "host_request", host_with_override):
            url, headers = _run(_Integration()._resolve_config())
            self.assertEqual(url, _Integration.url)
            self.assertEqual(headers, {})


if __name__ == "__main__":
    unittest.main()
