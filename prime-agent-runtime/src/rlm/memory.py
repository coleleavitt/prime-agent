"""Active memory: a file tree the model reads and writes through
``rlm.memory.*``. Modelled on Claude Code 2.1.318's ``memory_*`` tool
family. Entries live under ``<agent dir>/memory/personal/``, a single
tree per user shared across projects and sessions (separate from the
per-project ``MEMORY.md`` index the prompt ships).

Paths are relative POSIX strings, NFC-normalized, at most 20 segments
and 1024 bytes, no ``.`` / ``..`` / control characters. File contents
are capped at 100 KiB. Violations raise :class:`MemoryError` with a
stable ``code`` attribute (``invalid_path``, ``content_too_large``,
``not_found``, ``already_exists``, ``replace_not_unique``, ...).

Every call returns a plain dict; the host serves the ops through
``memory.read`` / ``memory.write`` / ``memory.append`` /
``memory.str_replace`` / ``memory.delete`` / ``memory.list`` /
``memory.search`` host requests.
"""

from __future__ import annotations

from typing import Any


class MemoryError(Exception):
    """A memory operation refused by the host.

    The :attr:`code` matches the stable code the host reports
    (``invalid_path``, ``not_found``, ``content_too_large``, ...), so
    callers can branch on it without string-matching the message.
    """

    def __init__(self, message: str, *, code: str = "error") -> None:
        super().__init__(message)
        self.code = code


async def _request(op: str, payload: dict[str, Any]) -> dict[str, Any]:
    from . import host_request

    reply = await host_request(f"memory.{op}", payload)
    if isinstance(reply, dict) and "error" in reply:
        raise MemoryError(str(reply["error"]), code=str(reply.get("code", "error")))
    return reply if isinstance(reply, dict) else {}


class _MemoryNamespace:
    """Namespace for ``rlm.memory`` functions."""

    async def read(self, path: str) -> str:
        """Return the file at ``path`` as a string."""
        reply = await _request("read", {"path": path})
        return str(reply.get("content", ""))

    async def write(self, path: str, content: str) -> dict[str, Any]:
        """Replace ``path`` with ``content``, creating it if needed.

        Returns ``{"bytes": N}`` with the written size.
        """
        return await _request("write", {"path": path, "content": content})

    async def append(self, path: str, content: str) -> dict[str, Any]:
        """Append ``content`` to ``path``, creating it if needed."""
        return await _request("append", {"path": path, "content": content})

    async def str_replace(self, path: str, old: str, new: str) -> dict[str, Any]:
        """Replace the single occurrence of ``old`` in ``path`` with ``new``.

        Refuses if ``old`` does not match exactly once (``code =
        "replace_not_unique"``); use :meth:`read` and :meth:`write` for
        multi-match edits.
        """
        return await _request(
            "str_replace", {"path": path, "old": old, "new": new}
        )

    async def delete(self, path: str) -> dict[str, Any]:
        """Remove ``path``. Empty parent directories are pruned up to the root."""
        return await _request("delete", {"path": path})

    async def list(self, prefix: str | None = None) -> list[dict[str, Any]]:
        """Return the entries at or below ``prefix``.

        Each entry is ``{"path": str, "size": int, "modified_unix": int}``
        (the last field is absent when the OS did not supply it). An empty
        or missing ``prefix`` returns the whole tree.
        """
        payload: dict[str, Any] = {}
        if prefix:
            payload["prefix"] = prefix
        reply = await _request("list", payload)
        entries = reply.get("entries", [])
        return list(entries) if isinstance(entries, list) else []

    async def search(
        self, query: str, *, limit: int | None = None
    ) -> list[dict[str, Any]]:
        """Return the lines containing ``query`` across every file.

        Case-sensitive literal match (no regex). Each hit is ``{"path":
        str, "line_number": int, "line": str}``, sorted by path then line.
        ``limit`` caps the number of hits (default 100, max 500).
        """
        payload: dict[str, Any] = {"query": query}
        if limit is not None:
            payload["limit"] = int(limit)
        reply = await _request("search", payload)
        hits = reply.get("hits", [])
        return list(hits) if isinstance(hits, list) else []

    def __repr__(self) -> str:
        return "rlm.memory"


memory = _MemoryNamespace()

__all__ = ["MemoryError", "memory"]
