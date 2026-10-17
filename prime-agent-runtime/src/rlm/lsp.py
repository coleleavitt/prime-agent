"""Code intelligence from language servers (Claude Code's LSP tool).

``rlm.lsp.definition(path, line, col)`` and its siblings ask the language
server configured for the file's extension (the ``lspServers`` setting, or a
trusted project's ``.lsp.json``); the host starts it on first use. Lines and
columns are 1-based, as editors show them. Each call returns an
``LspResult``: ``print(result)`` shows Claude Code's text, ``.items`` holds
the same results as dicts (``path``, ``line``, ``character``, ...).

A file whose extension no server handles answers with ``available=False``
and says so in ``.text``. The calls are served by the host's ``lsp`` feature
(``lsp.*`` host requests); a build without it raises the standard
unregistered-request error.

After a cell writes files, new diagnostics from their servers arrive on their
own as a ``<new-diagnostics>`` reminder; ``diagnostics(path)`` reads a file's
current ones on demand.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from typing import Any

_DIRECTIONS = ("incoming", "outgoing", "prepare")


@dataclass(frozen=True)
class LspResult:
    """One LSP answer: ``text`` is what to read, ``items`` the results as data."""

    operation: str
    text: str
    path: str | None = None
    server: str | None = None
    available: bool = True
    items: list[dict[str, Any]] = field(default_factory=list)
    result_count: int = 0
    file_count: int = 0

    def __str__(self) -> str:
        return self.text


def _path(path: Any) -> str:
    if not isinstance(path, (str, os.PathLike)):
        raise TypeError(f"path must be str or os.PathLike, got {type(path).__name__}")
    text = os.fspath(path)
    if not isinstance(text, str) or not text.strip():
        raise ValueError("path must be a non-empty path")
    return os.path.abspath(text)


def _position(name: str, value: Any) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise ValueError(f"{name} must be a positive int (1-based, as shown in editors)")
    return value


def _int(value: Any) -> int:
    return value if isinstance(value, int) and not isinstance(value, bool) else 0


async def _ask(request_type: str, payload: dict[str, Any]) -> LspResult:
    from . import host_request

    reply = await host_request(request_type, payload)
    return LspResult(
        operation=str(reply.get("operation") or request_type),
        text=str(reply.get("text") or ""),
        path=reply.get("path") if isinstance(reply.get("path"), str) else None,
        server=reply.get("server") if isinstance(reply.get("server"), str) else None,
        available=reply.get("available") is not False,
        items=[item for item in reply.get("items") or [] if isinstance(item, dict)],
        result_count=_int(reply.get("result_count")),
        file_count=_int(reply.get("file_count")),
    )


async def _at(request_type: str, path: Any, line: Any, col: Any, **extra: Any) -> LspResult:
    payload: dict[str, Any] = {
        "path": _path(path),
        "line": _position("line", line),
        "character": _position("col", col),
    }
    payload.update(extra)
    return await _ask(request_type, payload)


async def definition(path: str, line: int, col: int) -> LspResult:
    """Where the symbol at ``path:line:col`` is defined."""
    return await _at("lsp.definition", path, line, col)


async def implementation(path: str, line: int, col: int) -> LspResult:
    """The implementations of the interface or abstract method at ``path:line:col``."""
    return await _at("lsp.implementation", path, line, col)


async def references(path: str, line: int, col: int, *, include_declaration: bool = True) -> LspResult:
    """Every reference to the symbol at ``path:line:col`` (git-ignored files left out)."""
    return await _at("lsp.references", path, line, col, include_declaration=bool(include_declaration))


async def hover(path: str, line: int, col: int) -> LspResult:
    """The server's hover text (type, signature, docs) for the symbol at ``path:line:col``."""
    return await _at("lsp.hover", path, line, col)


async def document_symbols(path: str) -> LspResult:
    """The symbols (functions, classes, variables) of one file, nested."""
    return await _ask("lsp.document_symbols", {"path": _path(path)})


async def workspace_symbols(query: str, path: str | None = None) -> LspResult:
    """Symbols matching ``query`` across the workspace.

    ``path`` (any file of the language) picks and starts that language's
    server; without it every running server is asked.
    """
    if not isinstance(query, str) or not query.strip():
        raise ValueError("query must be a non-empty symbol name or part of one")
    payload: dict[str, Any] = {"query": query}
    if path is not None:
        payload["path"] = _path(path)
    return await _ask("lsp.workspace_symbols", payload)


async def call_hierarchy(path: str, line: int, col: int, direction: str = "incoming") -> LspResult:
    """The callers (``incoming``) or callees (``outgoing``) of the function at
    ``path:line:col``, or just its call hierarchy item (``prepare``)."""
    if direction not in _DIRECTIONS:
        raise ValueError(f"direction must be one of {', '.join(_DIRECTIONS)}")
    return await _at("lsp.call_hierarchy", path, line, col, direction=direction)


async def diagnostics(path: str) -> LspResult:
    """The file's current diagnostics from its server (opening the file when needed)."""
    return await _ask("lsp.diagnostics", {"path": _path(path)})


async def servers() -> dict[str, Any]:
    """The configured servers (name, command, extensions, status) and configuration errors."""
    from . import host_request

    return await host_request("lsp.servers", {})
