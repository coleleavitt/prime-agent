"""Prime Agent ctx skill: manage this session's context window.

The context manager runs host-side (the `context` feature): it tags user
messages and tool outputs `§N§`, renders dropped items as `[dropped §N§]`,
and folds old history into `<session-history>`. This is a thin wrapper over
the generic host bridge (`rlm.host_request`); it only works inside the
Prime Agent Python kernel.
"""

from __future__ import annotations

from typing import Any

from rlm import host_request


def _spec(tags: Any) -> str:
    """`tags` as the host's spec: a string like "3-5,9", one tag (`12` or
    "§12§"), or an iterable of them (a list, a range)."""
    if isinstance(tags, str):
        return tags
    if isinstance(tags, int) and not isinstance(tags, bool):
        return str(tags)
    try:
        items = list(tags)
    except TypeError as error:
        raise TypeError('tags must be a string like "3-5,9", an int, or a list or range of tags') from error
    return ",".join(str(item).strip().strip("§") for item in items)


async def reduce(tags: Any) -> dict[str, Any]:
    """Queue drops for tags: "3-5,9", 12, "§12§", [3, 4, "§9§"] or range(3, 6).
    Returns `queued` and `skipped` (with reasons)."""
    spec = _spec(tags)
    if not spec.strip():
        raise ValueError('tags must name at least one tag, like "3-5,9"')
    reply: dict[str, Any] = await host_request("ctx.reduce", {"tags": spec})
    return dict(reply)


async def expand(
    tag: int | None = None, start: int | None = None, end: int | None = None
) -> str:
    """One item's original text (`tag`), or the raw transcript `start`..`end`."""
    if tag is not None:
        payload: dict[str, Any] = {"tag": int(tag)}
    elif start is not None and end is not None:
        payload = {"start": int(start), "end": int(end)}
    else:
        raise ValueError("pass tag, or both start and end")
    reply: dict[str, Any] = await host_request("ctx.expand", payload)
    return str(reply.get("text", ""))


async def status() -> dict[str, Any]:
    """Usage, window, biggest undropped outputs, queued drops, compartments."""
    reply: dict[str, Any] = await host_request("ctx.status", {})
    return dict(reply)


async def flush() -> dict[str, Any]:
    """Apply queued drops on the next request (one prompt-cache rebuild)."""
    reply: dict[str, Any] = await host_request("ctx.flush", {})
    return dict(reply)
