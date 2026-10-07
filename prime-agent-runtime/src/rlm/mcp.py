"""Kernel-side client of the host-owned MCP sessions.

The MCP connections themselves (stdio servers, streamable HTTP, credentials,
timeouts, the stderr tail of a failing stdio server) live in the Prime Agent
host, one set per agent session, so they survive kernel restarts. This module
is the kernel's thin client over them; its API is unchanged:

- Dispatch: ``list_tools(connection)`` / ``call_tool(connection, tool, arguments)``
  open a configured MCP server (host-resolved), discover its tools, and call
  them. Adding a service is data, not a new Python module.
- Discovery: ``list_plugins`` / ``search_plugins`` (host-owned catalog),
  ``list_connections`` (the user's actual connections), ``search_tools`` /
  ``describe_tool`` (live tool metadata). Inventory calls are bounded and never
  return credentials; live tool schemas and results are passed through
  unmodified.
- Host view: ``status(servers, timeout_ms)`` feeds the daemon's MCP
  connections view (one bounded per-server listing, errors reported per
  server).

Cancelling a call (a kernel interrupt, a ``wait_for`` timeout) cancels the
host-side request too: an in-flight ``tools/call`` is abandoned and the server
is told.
"""

from __future__ import annotations

import asyncio
import re
from typing import Any

from . import host_request, trace
from .mcp_base import McpToolError

__all__ = [
    "McpCredentialsUnavailable",
    "McpDiscoveryError",
    "McpStartupError",
    "McpToolError",
    "call_tool",
    "close",
    "describe_tool",
    "list_connections",
    "list_plugins",
    "list_tools",
    "reload",
    "search_plugins",
    "search_tools",
]

# Bound on the host closing connections (its per-connection bound is 2.5s).
_SHUTDOWN_TIMEOUT = 2.5
# How long a cancelled request may take to settle host-side.
_CANCEL_DRAIN_MS = 5_000
# Host-backed inventory requests are interactive-sized, not tool-call-sized.
_INVENTORY_TIMEOUT = 15.0
_DEFAULT_PLUGIN_LIMIT = 50
_MAX_PLUGIN_LIMIT = 200
_DEFAULT_PLUGIN_SEARCH_LIMIT = 10
_MAX_PLUGIN_SEARCH_LIMIT = 50
_MAX_CURSOR_CHARS = 512
_PLUGIN_CONNECTION_STATUSES = ("connected", "not_connected")
_DEFAULT_TOOL_SEARCH_LIMIT = 20
_MAX_TOOL_SEARCH_LIMIT = 50
_MAX_TOOL_SEARCH_SERVERS = 8
# Defensive only: the host must already whitelist safe metadata in inventory
# entries. Never applied to live tool schemas or results — argument names there
# are server-defined and legitimately credential-like.
_SECRET_KEY_PATTERN = re.compile(r"token|secret|password|credential|authorization|api[_-]?key|private[_-]?key", re.I)
# Host view markers whose names match the pattern but whose boolean value is
# never a credential: `pasteToken: true` marks the rows the user connects by
# pasting a token in `/plugins`. Kept only when the value is a boolean, so a
# string smuggled under the same key is still dropped.
_BOOLEAN_MARKER_KEYS = frozenset({"pasteToken"})


class McpStartupError(RuntimeError):
    """A stdio server failed while completing the MCP startup handshake."""


class McpDiscoveryError(RuntimeError):
    """Raised when a server's tools/list pagination cannot complete honestly.

    A repeated or malformed pagination cursor, or more pages than allowed,
    means the inventory cannot be trusted — a partial tool map is never
    published as if complete."""


class McpCredentialsUnavailable(RuntimeError):
    """Raised when a configured connection has no usable credentials.

    The user must connect the service first (`/plugins` or
    `/mcp login <service>`)."""


# The exception classes the host names in a failed reply.
_ERROR_TYPES: dict[str, type[BaseException]] = {
    "RuntimeError": RuntimeError,
    "KeyError": KeyError,
    "PermissionError": PermissionError,
    "ValueError": ValueError,
    "TimeoutError": TimeoutError,
    "FileNotFoundError": FileNotFoundError,
    "OSError": OSError,
    "McpStartupError": McpStartupError,
    "McpDiscoveryError": McpDiscoveryError,
    "McpCredentialsUnavailable": McpCredentialsUnavailable,
    "McpToolError": McpToolError,
}


class _Client:
    """Kernel-side state: whether ``close()`` shut this kernel's client down."""

    def __init__(self) -> None:
        self.shut_down = False

    def accepting_work(self) -> None:
        if self.shut_down:
            raise RuntimeError("MCP registry is shut down")


_client = _Client()


def _consume_exception(task: asyncio.Task[Any]) -> None:
    if not task.cancelled():
        task.exception()


async def _session_request(
    request_type: str, payload: dict[str, Any]
) -> tuple[Any, bool, BaseException | None]:
    """One ``mcp.session.*`` / ``mcp.integration.*`` host request.

    Returns ``(value, connected, error)``: ``error`` is the exception the
    host named (to raise), ``connected`` whether a live connection existed
    when the request began. Cancelling the caller cancels the host request
    (``host_cancel``) and propagates at once; the request settles host-side
    in the background.
    """
    from . import repl

    data = {**payload, "type": request_type}
    request = asyncio.ensure_future(
        repl.host_request(data, cancel_on_cancel=True, drain_timeout_ms=_CANCEL_DRAIN_MS)
    )
    try:
        raw = await asyncio.shield(request)
    except asyncio.CancelledError:
        request.cancel()
        request.add_done_callback(_consume_exception)
        raise
    if not isinstance(raw, dict) or raw.get("status") != "ok":
        error = raw.get("error") if isinstance(raw, dict) else None
        raise RuntimeError(str(error or f"host request {request_type} failed"))
    result = raw.get("result")
    if not isinstance(result, dict):
        raise RuntimeError(f"host request {request_type} returned a malformed response")
    connected = result.get("connected") is True
    if result.get("ok") is True:
        return result.get("value"), connected, None
    error = result.get("error") if isinstance(result.get("error"), dict) else {}
    kind = error.get("type")
    message = str(error.get("message") or "")
    if kind == "CancelledError":
        return None, connected, asyncio.CancelledError()
    exc_type = _ERROR_TYPES.get(kind, RuntimeError)
    return None, connected, exc_type(message) if message else exc_type()


async def _server_request(
    request_type: str, server: str, payload: dict[str, Any], span: Any = None
) -> Any:
    """A host request about one server; records ``mcp.connected`` on ``span``."""
    _client.accepting_work()
    _validate_name(server, "server")
    value, connected, error = await _session_request(request_type, {**payload, "server": server})
    if span is not None:
        span.attrs["mcp.connected"] = connected
    if error is not None:
        raise error
    return value


async def list_tools(server: str) -> list[dict[str, Any]]:
    # One "mcp.call" span per public call (see repl.md "Trace context"); a lazy
    # connect/spawn inside the call is simply part of the span's duration.
    with trace.start_span("mcp.call", **_span_attrs(server, "list_tools")) as span:
        tools = await _server_request("mcp.session.list_tools", server, {}, span)
        span.attrs["mcp.tool_count"] = len(tools)
        return tools


async def status(servers: list[str], timeout_ms: float) -> list[dict[str, Any]]:
    """Per-server tool listing for the host's MCP connections view.

    Each requested server is listed concurrently, bounded by `timeout_ms`
    per server; a server that fails or times out reports its error instead
    of failing the whole request. Opening a not-yet-connected server is
    intended: the view exists to show what each connection offers.
    """
    timeout = max(timeout_ms, 1.0) / 1000.0

    async def _one(server: str) -> dict[str, Any]:
        try:
            tools = await asyncio.wait_for(list_tools(server), timeout=timeout)
        except BaseException as exc:  # noqa: BLE001 - one broken server reports alone
            return {"server": server, "tools": None, "error": f"{type(exc).__name__}: {exc}"}
        return {
            "server": server,
            "tools": [
                {"name": tool.get("name"), "description": tool.get("description") or ""}
                for tool in tools
            ],
            "error": None,
        }

    results = await asyncio.gather(*(_one(server) for server in servers))
    return list(results)


async def call_tool(server: str, tool: str, arguments: dict[str, Any] | None = None) -> Any:
    with trace.start_span("mcp.call", **_span_attrs(server, tool)) as span:
        _validate_name(tool, "tool")
        if arguments is not None and not isinstance(arguments, dict):
            raise TypeError("arguments must be a dict or None")
        return await _server_request(
            "mcp.session.call_tool", server, {"tool": tool, "arguments": arguments or {}}, span
        )


def _span_attrs(server: Any, tool: Any) -> dict[str, Any]:
    """Attributes for an ``mcp.call`` span; never raises (names may still be invalid).

    ``mcp.connected`` starts False and is set from the host's reply: True
    means a live connection existed when the call began, False that the call
    (re)connected the server lazily, so its duration includes the startup
    handshake.
    """
    return {
        "mcp.server": server if isinstance(server, str) else repr(server),
        "mcp.tool": tool if isinstance(tool, str) else repr(tool),
        "mcp.connected": False,
    }


async def reload(server: str | None = None) -> None:
    _client.accepting_work()
    payload: dict[str, Any] = {}
    if server is not None:
        _validate_name(server, "server")
        payload["server"] = server
    await _bounded_close("mcp.session.reload", payload)


async def close() -> None:
    """Close this session's MCP connections and shut this kernel's client down."""
    if _client.shut_down:
        return
    _client.shut_down = True
    await _bounded_close("mcp.session.close", {})


async def _bounded_close(request_type: str, payload: dict[str, Any]) -> None:
    try:
        _, _, error = await asyncio.wait_for(
            _session_request(request_type, payload), timeout=_SHUTDOWN_TIMEOUT + 1
        )
    except TimeoutError:
        raise RuntimeError("Timed out waiting for MCP connections to close") from None
    if error is not None:
        raise error


# -- discovery / inventory surface ---------------------------------------------


async def list_connections() -> list[dict[str, Any]]:
    """The user's current MCP connections (host-owned records, no secrets).

    Each entry carries at least ``connectionId``; other fields are host-defined
    (label, service, status, account). ``connectionId`` is the name
    ``list_tools``/``call_tool`` accept.
    """
    result = await _host_inventory("mcp.list_connections", {})
    connections = _inventory_entries(result, "connections")
    for entry in connections:
        connection_id = entry.get("connectionId")
        if not isinstance(connection_id, str) or not connection_id:
            raise RuntimeError("MCP connection inventory from the host is malformed")
    return [_sanitize_inventory_entry(entry) for entry in connections]


async def list_plugins(
    connection_status: str | None = None,
    limit: int = _DEFAULT_PLUGIN_LIMIT,
    cursor: str | None = None,
) -> dict[str, Any]:
    """One bounded page of the supported-service catalog (host-owned).

    Returns ``{"plugins": [...], "nextCursor": str | None}``; while
    ``nextCursor`` is not None, more entries exist — pass it back as ``cursor``.
    ``connection_status`` filters to ``"connected"`` or ``"not_connected"``.
    """
    if connection_status is not None and connection_status not in _PLUGIN_CONNECTION_STATUSES:
        raise ValueError("connection_status must be None, 'connected' or 'not_connected'")
    _validate_limit(limit, "limit", _MAX_PLUGIN_LIMIT)
    payload: dict[str, Any] = {"limit": limit}
    if connection_status is not None:
        payload["connectionStatus"] = connection_status
    if cursor is not None:
        _validate_cursor(cursor)
        payload["cursor"] = cursor
    result = await _host_inventory("mcp.list_plugins", payload)
    return _plugin_page(result)


async def search_plugins(query: str, limit: int = _DEFAULT_PLUGIN_SEARCH_LIMIT) -> dict[str, Any]:
    """Bounded search of the supported-service catalog by the host.

    Matches service ids, labels, aliases, descriptions, categories and
    publishers. Returns ``{"plugins": [...], "nextCursor": None}``; narrow the
    query rather than expecting exhaustive pages.
    """
    if not isinstance(query, str) or not query.strip():
        raise TypeError("query must be a non-empty string")
    _validate_limit(limit, "limit", _MAX_PLUGIN_SEARCH_LIMIT)
    payload: dict[str, Any] = {"query": query.strip(), "limit": limit}
    result = await _host_inventory("mcp.search_plugins", payload)
    return _plugin_page(result)


async def search_tools(
    query: str,
    connection_id: str | None = None,
    limit: int = _DEFAULT_TOOL_SEARCH_LIMIT,
) -> dict[str, Any]:
    """Search live tool names/descriptions and report the search scope.

    With ``connection_id`` only that connection is searched and its errors
    propagate. Without it, at most ``_MAX_TOOL_SEARCH_SERVERS`` connections the
    host reports as connected are searched in host order; per-connection
    failures are reported as fixed, redaction-safe summaries, not raised.

    Returns ``{"tools": [{connectionId, name, description}, ...], "searched":
    [connectionId, ...], "unavailable": [{connectionId, error}, ...],
    "truncated": bool}``. ``truncated`` means matches or servers may remain —
    narrow the query or search a specific connection.
    """
    if not isinstance(query, str) or not query.strip():
        raise TypeError("query must be a non-empty string")
    _validate_limit(limit, "limit", _MAX_TOOL_SEARCH_LIMIT)
    needle = query.strip()
    if connection_id is not None:
        _validate_name(connection_id, "connection")
        tools = await _search(connection_id, needle, limit)
        return {"tools": tools, "searched": [connection_id], "unavailable": [], "truncated": len(tools) >= limit}
    candidates = [
        entry["connectionId"]
        for entry in await list_connections()
        if entry.get("status") == "connected"
    ]
    scoped = candidates[:_MAX_TOOL_SEARCH_SERVERS]
    tools: list[dict[str, Any]] = []
    searched: list[str] = []
    unavailable: list[dict[str, Any]] = []
    for connection in scoped:
        try:
            found = await _search(connection, needle, limit)
        except Exception as exc:
            unavailable.append({"connectionId": connection, "error": _bounded_error(exc)})
            continue
        searched.append(connection)
        if len(tools) < limit:
            tools.extend(found[: limit - len(tools)])
    truncated = len(tools) >= limit or len(candidates) > len(scoped)
    return {"tools": tools, "searched": searched, "unavailable": unavailable, "truncated": truncated}


async def describe_tool(connection_id: str, tool: str) -> dict[str, Any]:
    """One live tool's ``{"name", "description", "inputSchema"}``.

    Raises ``KeyError`` when the tool or connection is unknown and
    ``PermissionError`` when policy (``enabledTools``/``disabledTools``)
    excludes the tool. Schemas pass through unmodified.
    """
    _validate_name(connection_id, "connection")
    _validate_name(tool, "tool")
    return await _server_request("mcp.session.describe_tool", connection_id, {"tool": tool})


async def _search(connection_id: str, needle: str, limit: int) -> list[dict[str, Any]]:
    return await _server_request(
        "mcp.session.search_tools", connection_id, {"query": needle, "limit": limit}
    )


async def _host_inventory(request_type: str, payload: dict[str, Any]) -> dict[str, Any]:
    outcome: str | None = None
    try:
        async with asyncio.timeout(_INVENTORY_TIMEOUT):
            result = await host_request(request_type, payload)
    except TimeoutError:
        outcome = "timed out"
    except Exception:
        outcome = "failed"
    if outcome is not None:
        # Raised outside the handler so the original exception is not chained:
        # arbitrary host/bridge error text (which can embed credential-bearing
        # URLs, query strings, headers and HTTP bodies) is never echoed, and
        # not even reachable through __cause__/__context__.
        raise RuntimeError(f"MCP {request_type} request {outcome}")
    if not isinstance(result, dict):
        raise RuntimeError(f"MCP {request_type} returned a malformed response")
    return result


def _inventory_entries(result: dict[str, Any], key: str) -> list[dict[str, Any]]:
    entries = result.get(key)
    if not isinstance(entries, list) or not all(isinstance(entry, dict) for entry in entries):
        raise RuntimeError(f"MCP {key} inventory from the host is malformed")
    return entries


def _plugin_page(result: dict[str, Any]) -> dict[str, Any]:
    entries = _inventory_entries(result, "plugins")
    next_cursor = result.get("nextCursor")
    if next_cursor is not None and (not isinstance(next_cursor, str) or not next_cursor):
        raise RuntimeError("MCP plugin inventory from the host is malformed")
    return {
        "plugins": [_sanitize_inventory_entry(entry) for entry in entries],
        "nextCursor": next_cursor,
    }


def _is_secret_key(key: Any, value: Any) -> bool:
    """Whether one inventory key is dropped as secret-looking (allowlisted boolean markers are kept)."""
    if not isinstance(key, str):
        return False
    if key in _BOOLEAN_MARKER_KEYS and isinstance(value, bool):
        return False
    return bool(_SECRET_KEY_PATTERN.search(key))


def _sanitize_inventory_entry(entry: dict[str, Any]) -> dict[str, Any]:
    """Copy one inventory entry, dropping secret-looking keys defensively.

    The host must already whitelist safe metadata; this is a second line of
    defense for catalog and connection records only. Never applied to live tool
    schemas or tool results.
    """
    cleaned: dict[str, Any] = {}
    for key, value in entry.items():
        if _is_secret_key(key, value):
            continue
        cleaned[key] = _sanitize_inventory_value(value)
    return cleaned


def _sanitize_inventory_value(value: Any) -> Any:
    if isinstance(value, dict):
        return _sanitize_inventory_entry(value)
    if isinstance(value, list):
        return [_sanitize_inventory_value(item) for item in value]
    return value


_SEARCH_FAILURE_HINTS: tuple[tuple[type[BaseException], str], ...] = (
    (McpCredentialsUnavailable, "credentials for this connection are not available; the user must connect it"),
    (McpStartupError, "the MCP server failed during startup"),
    (KeyError, "the connection or tool is not declared"),
    (PermissionError, "policy excludes this connection or tool"),
    (TimeoutError, "opening or querying the connection timed out"),
)


def _bounded_error(exc: BaseException) -> str:
    """A fixed, redaction-safe summary of one connection's search failure.

    Raw exception text can embed credential-bearing URLs and HTTP bodies, so
    only the exception type name and a fixed hint are ever surfaced.
    """
    hint = "the connection could not be opened or searched"
    for failure_type, message in _SEARCH_FAILURE_HINTS:
        if isinstance(exc, failure_type):
            hint = message
            break
    return f"{type(exc).__name__}: {hint}"



def _validate_limit(value: Any, label: str, maximum: int) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or not 1 <= value <= maximum:
        raise ValueError(f"{label} must be an integer between 1 and {maximum}")


def _validate_cursor(value: Any) -> None:
    if not isinstance(value, str) or not value or len(value) > _MAX_CURSOR_CHARS:
        raise ValueError(f"cursor must be a non-empty string of at most {_MAX_CURSOR_CHARS} characters")


def _validate_name(value: str, label: str) -> None:
    if not isinstance(value, str) or not value:
        raise TypeError(f"{label} must be a non-empty string")
