"""Monitors: a background command's output lines as events to the model.

``rlm.monitor(source)`` follows a command's output line by line, and
``rlm.monitor(url=...)`` a WebSocket's messages (Claude Code's Monitor tool).
Each batch of new lines becomes a ``monitor.event`` host request; the host
queues it as a notice that joins a running turn at its next boundary or starts
a turn on an idle session. The monitor ends when the command exits, the socket
closes, its timeout passes (never, for a persistent one), or on
``rlm.monitor_stop(id)``; that end is one last event. Lines can be filtered
with a regular expression.

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

from . import _telemetry, _websocket
from .bash import BashHandle, _raise_for, _request, bash

_POLL_SECONDS = 0.2
_LINE_CHARS = 500
_BATCH_CHARS = 3000
_BUCKET_CAPACITY = 10
_BUCKET_REFILL_SECONDS = 2.0
_FLOOD_STOP_SECONDS = 30.0
_DEFAULT_TIMEOUT_SECONDS = 300.0
_MAX_TIMEOUT_SECONDS = 3600.0
_CONNECT_SECONDS = 15.0
# Characters of socket messages held between two reads; past it they are skipped.
_SOCKET_BUFFER_CHARS = 1 << 20
# Polls with no new output after the command exited before the end event:
# the output pump may still be copying the last bytes when the reap lands.
_DRAIN_QUIET_POLLS = 2

_ids = itertools.count(1)


class _CommandSource:
    """A command's output: one the monitor started, or a ``bash()`` handle's new output."""

    def __init__(self, handle: BashHandle, *, owned: bool, cursor: int) -> None:
        self.handle = handle
        self.owned = owned
        self.cursor = cursor

    @property
    def pid(self) -> int:
        return self.handle.pid

    @property
    def running(self) -> bool:
        return self.handle.running

    def read(self) -> tuple[str, int]:
        """The output since the last read, and how many bytes were skipped."""
        reply = _read_since(self.handle, self.cursor)
        self.cursor = int(reply["to"])
        return str(reply.get("output", "")), int(reply.get("skipped", 0))

    def end(self) -> dict[str, Any]:
        return {"reason": "exited", "exitCode": _exit_code(self.handle)}

    async def close(self) -> None:
        if self.owned and self.handle.running:
            self.handle.kill()


class _SocketSource:
    """A WebSocket's messages, one or more lines each, buffered between reads."""

    def __init__(self, socket: _websocket.WebSocket) -> None:
        self.socket = socket
        self.pid = None
        self.closed: _websocket.Closed | None = None
        self.error: str | None = None
        self._lines: list[str] = []
        self._size = 0
        self._skipped = 0
        self._pump_task = asyncio.get_running_loop().create_task(self._pump())

    @property
    def running(self) -> bool:
        return not self._pump_task.done()

    async def _pump(self) -> None:
        try:
            while True:
                message = await self.socket.receive()
                if isinstance(message, _websocket.Closed):
                    self.closed = message
                    return
                text = message if isinstance(message, str) else f"[binary message, {len(message)} bytes]"
                if self._size + len(text) > _SOCKET_BUFFER_CHARS:
                    self._skipped += len(text)
                else:
                    self._lines.append(text)
                    self._size += len(text)
        except _websocket.WebSocketError as error:
            self.error = str(error)

    def read(self) -> tuple[str, int]:
        text = "".join(f"{message.rstrip(chr(10))}\n" for message in self._lines)
        skipped, self._lines, self._size, self._skipped = self._skipped, [], 0, 0
        return text, skipped

    def end(self) -> dict[str, Any]:
        if self.error is not None:
            return {"reason": "failed", "error": self.error[:300]}
        return {"reason": "closed", "code": None if self.closed is None else self.closed.code}

    async def close(self) -> None:
        self._pump_task.cancel()
        await self.socket.close()


@dataclass
class _Monitor:
    id: str
    description: str
    source: _CommandSource | _SocketSource
    pattern: re.Pattern[str] | None
    deadline: float | None
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
            text, skipped = monitor.source.read()
            if skipped:
                pending.append(f"[{skipped} bytes of output skipped: too much to follow]")
            partial += text
            *complete, partial = partial.split("\n")
            for line in complete:
                line = line.rstrip("\r")
                if monitor.pattern is None or monitor.pattern.search(line):
                    pending.append(_clip(line))
            running = monitor.source.running
            if not running:
                quiet_after_exit = 0 if text else quiet_after_exit + 1
            if not running and quiet_after_exit >= _DRAIN_QUIET_POLLS:
                if partial and (monitor.pattern is None or monitor.pattern.search(partial)):
                    pending.append(_clip(partial))
                end = monitor.source.end()
                break
            if monitor.deadline is not None and now >= monitor.deadline:
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
        await monitor.source.close()
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
    source: str | BashHandle | None = None,
    *,
    url: str | None = None,
    description: str | None = None,
    pattern: str | None = None,
    timeout_seconds: float = _DEFAULT_TIMEOUT_SECONDS,
    persistent: bool = False,
) -> dict[str, Any]:
    """Follow a command's output lines or a WebSocket's messages as events you are notified of.

    ``source`` is a shell command (started in the background and stopped with
    the monitor) or a handle from ``bash()`` (only its new output is followed;
    the command keeps running after the monitor ends). ``url`` is a ``ws://``
    or ``wss://`` address instead: each text message is an event (its lines),
    a binary message a placeholder line, and the socket closing ends the watch.
    Pass one of the two. Each batch of new lines arrives as a ``[monitor ...]``
    notice, between your tool calls while you work, or as a new turn once you
    have replied; do not poll or sleep for it. ``pattern`` keeps only matching
    lines (a regular expression). The monitor ends when the command exits or
    the socket closes, after ``timeout_seconds`` (default 300, at most 3600),
    or on ``rlm.monitor_stop(id)``, with one last notice. ``persistent=True``
    removes the timeout (``timeout_seconds`` is then ignored): the monitor runs
    until it is stopped, the source ends, or the kernel restarts.
    """
    if (source is None) == (url is None):
        raise ValueError("pass exactly one of source (a command or a bash() handle) or url (a WebSocket address)")
    if not persistent and (timeout_seconds <= 0 or timeout_seconds > _MAX_TIMEOUT_SECONDS):
        raise ValueError(f"timeout_seconds must be in (0, {int(_MAX_TIMEOUT_SECONDS)}]")
    compiled = re.compile(pattern) if pattern is not None else None
    stream: _CommandSource | _SocketSource
    if url is not None:
        label = description or url
        stream = _SocketSource(await _websocket.connect(url, timeout=_CONNECT_SECONDS))
    elif isinstance(source, str):
        if not source.strip():
            raise ValueError("source must be a non-empty command or a bash() handle")
        handle = bash(source)
        # The monitor reports the exit itself: the command's own completion
        # notice would say the same thing twice.
        handle._suppress_completion_notice()
        label = description or source
        if not handle._activity_id:
            raise RuntimeError("the command did not start")
        _read_since(handle, 0)  # the host can follow lines: refuse now, not in the background
        stream = _CommandSource(handle, owned=True, cursor=0)
    elif isinstance(source, BashHandle):
        label = description or source.command
        if not source._activity_id:
            raise RuntimeError("the command did not start")
        cursor = int(_read_since(source, source.peek_output_bytes())["to"])
        stream = _CommandSource(source, owned=False, cursor=cursor)
    else:
        raise TypeError("source must be a command string or a handle returned by bash()")
    entry = _Monitor(
        id=f"m{next(_ids)}",
        description=label[:200],
        source=stream,
        pattern=compiled,
        deadline=None if persistent else time.monotonic() + float(timeout_seconds),
    )
    entry.task = asyncio.get_running_loop().create_task(_run(entry))
    _MONITORS[entry.id] = entry
    _telemetry.emit(
        "monitor_started",
        source="websocket" if url is not None else "command" if isinstance(source, str) else "handle",
        persistent=persistent,
    )
    return {
        "id": entry.id,
        "description": entry.description,
        "pid": stream.pid,
        "timeout_seconds": None if persistent else float(timeout_seconds),
        "persistent": persistent,
        "note": "You will be notified of new lines and of the end. Keep working; do not poll or sleep.",
    }


def monitor_list() -> list[dict[str, Any]]:
    """The live monitors: id, description, pid (None for a WebSocket), whether persistent, events and lines delivered so far."""
    return [
        {
            "id": entry.id,
            "description": entry.description,
            "pid": entry.source.pid,
            "persistent": entry.deadline is None,
            "events": entry.events,
            "lines": entry.lines,
        }
        for entry in _MONITORS.values()
    ]


def monitor_stop(monitor_id: str) -> bool:
    """Stop a monitor (its command too, when the monitor started it; its socket). ``False`` when unknown."""
    entry = _MONITORS.get(monitor_id)
    if entry is None:
        return False
    entry.stop_reason = "stopped"
    return True
