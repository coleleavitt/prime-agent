"""Monitors: a background command's output lines as events to the model.

``rlm.monitor(source)`` follows a command's output line by line (Claude Code's
Monitor tool). Each batch of new lines becomes a ``monitor.event`` host request;
the host queues it as a notice that joins a running turn at its next boundary
or starts a turn on an idle session. The monitor ends when the command exits,
when its timeout passes, or on ``rlm.monitor_stop(id)``; that end is one last
event. Lines can be filtered with a regular expression.

Delivery is bounded the way Claude Code bounds it: new lines are batched over
200 ms, a line keeps at most 500 characters and a batch 3,000, and a token
bucket (10 batches, one more every 2 s) drops what floods past it, counting
the dropped lines. A monitor whose lines stay dropped for 30 s stops itself.
"""

from __future__ import annotations

import asyncio
import itertools
import re
import time
from dataclasses import dataclass, field
from typing import Any

from .bash import BashHandle, _raise_for, _request, bash

_POLL_SECONDS = 0.2
_LINE_CHARS = 500
_BATCH_CHARS = 3000
_BUCKET_CAPACITY = 10
_BUCKET_REFILL_SECONDS = 2.0
_FLOOD_STOP_SECONDS = 30.0
_DEFAULT_TIMEOUT_SECONDS = 300.0
_MAX_TIMEOUT_SECONDS = 3600.0
# Polls with no new output after the command exited before the end event:
# the output pump may still be copying the last bytes when the reap lands.
_DRAIN_QUIET_POLLS = 2

_ids = itertools.count(1)


@dataclass
class _Monitor:
    id: str
    description: str
    handle: BashHandle
    owns_command: bool
    pattern: re.Pattern[str] | None
    deadline: float
    cursor: int
    task: asyncio.Task[None] | None = None
    events: int = 0
    lines: int = 0
    stop_reason: str | None = None
    tokens: float = float(_BUCKET_CAPACITY)
    refilled_at: float = field(default_factory=time.monotonic)

    def take_token(self, now: float) -> bool:
        self.tokens = min(
            float(_BUCKET_CAPACITY), self.tokens + (now - self.refilled_at) / _BUCKET_REFILL_SECONDS
        )
        self.refilled_at = now
        if self.tokens >= 1.0:
            self.tokens -= 1.0
            return True
        return False


_MONITORS: dict[str, _Monitor] = {}


def _read_since(handle: BashHandle, cursor: int) -> dict[str, Any]:
    reply = _raise_for(_request({"type": "bash.output", "id": handle._activity_id, "sinceBytes": cursor}))
    if "to" not in reply:
        raise RuntimeError("this host cannot follow output by line; rlm.monitor needs a newer prime-agent")
    return reply


def _batch(lines: list[str]) -> tuple[list[str], int]:
    """The lines that fit one event, and how many did not."""
    kept: list[str] = []
    size = 0
    for line in lines:
        if kept and size + len(line) > _BATCH_CHARS:
            break
        kept.append(line)
        size += len(line) + 1
    return kept, len(lines) - len(kept)


def _clip(line: str) -> str:
    return line if len(line) <= _LINE_CHARS else line[:_LINE_CHARS] + " ..."


async def _send(monitor: _Monitor, lines: list[str], dropped: int, end: dict[str, Any] | None) -> None:
    from . import repl

    payload: dict[str, Any] = {
        "type": "monitor.event",
        "monitorId": monitor.id,
        "description": monitor.description,
        "lines": lines,
        "dropped": dropped,
    }
    if end is not None:
        payload["end"] = end
    await repl.host_request(payload)
    monitor.events += 1


def _exit_code(handle: BashHandle) -> int | None:
    # The finished result without reading it as the model would (that would
    # withdraw a supplied handle's own completion notice).
    result = handle._result
    return None if result is None else result.exit_code


async def _run(monitor: _Monitor) -> None:
    partial = ""
    pending: list[str] = []
    dropped = 0
    flooding_since: float | None = None
    quiet_after_exit = 0
    end: dict[str, Any] | None = None
    try:
        while True:
            await asyncio.sleep(_POLL_SECONDS)
            now = time.monotonic()
            if monitor.stop_reason is not None:
                end = {"reason": monitor.stop_reason}
                break
            read = _read_since(monitor.handle, monitor.cursor)
            monitor.cursor = int(read["to"])
            text = str(read.get("output", ""))
            if int(read.get("skipped", 0)):
                pending.append(f"[{int(read['skipped'])} bytes of output skipped: too much to follow]")
            partial += text
            *complete, partial = partial.split("\n")
            for line in complete:
                line = line.rstrip("\r")
                if monitor.pattern is None or monitor.pattern.search(line):
                    pending.append(_clip(line))
            running = monitor.handle.running
            if not running:
                quiet_after_exit = 0 if text else quiet_after_exit + 1
            if not running and quiet_after_exit >= _DRAIN_QUIET_POLLS:
                if partial and (monitor.pattern is None or monitor.pattern.search(partial)):
                    pending.append(_clip(partial))
                end = {"reason": "exited", "exitCode": _exit_code(monitor.handle)}
                break
            if now >= monitor.deadline:
                end = {"reason": "expired"}
                break
            if not pending:
                flooding_since = None
                continue
            if monitor.take_token(now):
                lines, over = _batch(pending)
                monitor.lines += len(lines)
                await _send(monitor, lines, dropped + over, None)
                pending, dropped, flooding_since = [], 0, None
            else:
                dropped += len(pending)
                pending = []
                flooding_since = flooding_since or now
                if now - flooding_since >= _FLOOD_STOP_SECONDS:
                    end = {"reason": "flooded"}
                    break
    except asyncio.CancelledError:
        end = {"reason": "stopped"}
    except Exception as error:  # noqa: BLE001 - a broken bridge ends the monitor, never the kernel
        end = {"reason": "failed", "error": str(error)[:300]}
    finally:
        _MONITORS.pop(monitor.id, None)
        if monitor.owns_command and monitor.handle.running:
            monitor.handle.kill()
    assert end is not None  # every exit from the loop above names its end
    lines, over = _batch(pending)
    monitor.lines += len(lines)
    end["events"] = monitor.events
    end["lines"] = monitor.lines
    try:
        await _send(monitor, lines, dropped + over, end)
    except Exception:  # noqa: BLE001 - nothing left to tell when the bridge is gone
        pass


async def monitor(
    source: str | BashHandle,
    *,
    description: str | None = None,
    pattern: str | None = None,
    timeout_seconds: float = _DEFAULT_TIMEOUT_SECONDS,
) -> dict[str, Any]:
    """Follow a command's output lines as events you are notified of.

    ``source`` is a shell command (started in the background and stopped with
    the monitor) or a handle from ``bash()`` (only its new output is followed;
    the command keeps running after the monitor ends). Each batch of new lines
    arrives as a ``[monitor ...]`` notice, between your tool calls while you
    work, or as a new turn once you have replied; do not poll or sleep for it.
    ``pattern`` keeps only matching lines (a regular expression). The monitor
    ends when the command exits, after ``timeout_seconds`` (default 300, at
    most 3600), or on ``rlm.monitor_stop(id)``, with one last notice.
    """
    if timeout_seconds <= 0 or timeout_seconds > _MAX_TIMEOUT_SECONDS:
        raise ValueError(f"timeout_seconds must be in (0, {int(_MAX_TIMEOUT_SECONDS)}]")
    compiled = re.compile(pattern) if pattern is not None else None
    if isinstance(source, str):
        if not source.strip():
            raise ValueError("source must be a non-empty command or a bash() handle")
        handle = bash(source)
        # The monitor reports the exit itself: the command's own completion
        # notice would say the same thing twice.
        handle._suppress_completion_notice()
        owns = True
        label = description or source
    elif isinstance(source, BashHandle):
        handle = source
        owns = False
        label = description or handle.command
    else:
        raise TypeError("source must be a command string or a handle returned by bash()")
    if not handle._activity_id:
        raise RuntimeError("the command did not start")
    cursor = 0 if owns else int(_read_since(handle, handle.peek_output_bytes())["to"])
    if owns:
        _read_since(handle, 0)  # the host can follow lines: refuse now, not in the background
    entry = _Monitor(
        id=f"m{next(_ids)}",
        description=label[:200],
        handle=handle,
        owns_command=owns,
        pattern=compiled,
        deadline=time.monotonic() + float(timeout_seconds),
        cursor=cursor,
    )
    entry.task = asyncio.get_running_loop().create_task(_run(entry))
    _MONITORS[entry.id] = entry
    return {
        "id": entry.id,
        "description": entry.description,
        "pid": handle.pid,
        "timeout_seconds": float(timeout_seconds),
        "note": "You will be notified of new lines and of the end. Keep working; do not poll or sleep.",
    }


def monitor_list() -> list[dict[str, Any]]:
    """The live monitors: id, description, pid, events and lines delivered so far."""
    return [
        {
            "id": entry.id,
            "description": entry.description,
            "pid": entry.handle.pid,
            "events": entry.events,
            "lines": entry.lines,
        }
        for entry in _MONITORS.values()
    ]


def monitor_stop(monitor_id: str) -> bool:
    """Stop a monitor (its command too, when the monitor started it). ``False`` when unknown."""
    entry = _MONITORS.get(monitor_id)
    if entry is None:
        return False
    entry.stop_reason = "stopped"
    return True
