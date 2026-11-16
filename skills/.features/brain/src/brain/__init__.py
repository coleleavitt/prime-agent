"""Prime Agent brain skill: one searchable view over every memory store.

The brain runs host-side (the retrieval feature): global and session-local
harness memories of every session, auto-memory and personal files, and
CortexKit magic-context's memories. This is a thin wrapper over the generic
host bridge (`rlm.host_request`); it only works inside the Prime Agent
Python kernel.
"""

from __future__ import annotations

from typing import Any

from rlm import host_request

_SCOPES = ("all", "project", "global", "session", "files", "magic-context")


def _key(key: str) -> str:
    if not isinstance(key, str) or not key.strip():
        raise ValueError("key must be a non-empty string (brain.search lists keys)")
    return key


async def search(
    query: str,
    scope: str = "all",
    limit: int = 10,
    include_superseded: bool = False,
) -> list[dict[str, Any]]:
    """Ranked memories for `query` (at most 25), best first.

    `scope` is one of all, project, global, session, files, magic-context.
    Superseded memories are left out unless `include_superseded` is true.
    """
    if not isinstance(query, str) or not query.strip():
        raise ValueError("query must be a non-empty string")
    if scope not in _SCOPES:
        raise ValueError(f"scope must be one of {', '.join(_SCOPES)}")
    reply: dict[str, Any] = await host_request(
        "brain.search",
        {
            "query": query,
            "scope": scope,
            "limit": int(limit),
            "includeSuperseded": bool(include_superseded),
        },
    )
    return list(reply.get("results", []))


async def get(key: str) -> dict[str, Any]:
    """One memory in full, with its pinned/excluded/superseded state."""
    reply: dict[str, Any] = await host_request("brain.get", {"key": _key(key)})
    return dict(reply.get("memory", {}))


async def _control(request: str, payload: dict[str, Any]) -> dict[str, Any]:
    reply: dict[str, Any] = await host_request(request, payload)
    return dict(reply.get("controls", {}))


async def pin(key: str) -> dict[str, Any]:
    """Pin a memory: it leads the digest and ranks higher in search."""
    return await _control("brain.pin", {"key": _key(key)})


async def unpin(key: str) -> dict[str, Any]:
    """Remove a pin."""
    return await _control("brain.unpin", {"key": _key(key)})


async def exclude(key: str) -> dict[str, Any]:
    """Exclude a memory from the digest and from search."""
    return await _control("brain.exclude", {"key": _key(key)})


async def include(key: str) -> dict[str, Any]:
    """Undo an exclusion."""
    return await _control("brain.include", {"key": _key(key)})


async def supersede(key: str, by: str | None = None) -> dict[str, Any]:
    """Record that `by` replaced `key`; with no `by`, clear the record."""
    payload: dict[str, Any] = {"key": _key(key)}
    if by is not None:
        payload["by"] = _key(by)
    return await _control("brain.supersede", payload)


async def summary() -> dict[str, Any]:
    """The synthesized summaries of this project and of all projects."""
    reply: dict[str, Any] = await host_request("brain.summary", {})
    return dict(reply)


async def synthesize(scope: str = "project") -> dict[str, Any]:
    """Consolidate the scope's memories (project or global) into its summary now."""
    if scope not in ("project", "global"):
        raise ValueError("scope must be project or global")
    reply: dict[str, Any] = await host_request("brain.synthesize", {"scope": scope})
    return dict(reply.get("summary", {}))


async def status() -> dict[str, Any]:
    """Memory counts per source, this project's identity, control counts."""
    reply: dict[str, Any] = await host_request("brain.status", {})
    return dict(reply)
