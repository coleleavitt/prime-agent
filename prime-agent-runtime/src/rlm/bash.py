"""Async-by-default shell execution: bash() spawns immediately and returns a live handle.

The checks and the process live in the Prime Agent host (the `pa-bash` crate):
the six refusal guards, process containment, the completion fence, the bounded
output buffer, progress events and the orphan-process journal. This module is
the kernel's client of it. Inside a Prime Agent kernel the requests travel as
kernel host requests; anywhere else (tests, scripts) they go to a
``prime-agent --prime-agent-bash-host`` sidecar this process starts on first
use. What stays here is what belongs to the kernel: the REPL cell lifecycle
(one-shot ownership of an awaited command, the background completion notice
and its withdrawal), the ``bash.command`` trace span, and plan mode's
classification of the kernel's own scripts.
"""

from __future__ import annotations

import asyncio
import atexit
import contextlib
import contextvars
import functools
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from collections.abc import Callable, Generator
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, NoReturn, final, overload

from . import plan_guard, trace

_IS_POSIX = os.name == "posix"

# Cancelled one-shot awaits: TERM grace before the group KILL, then the bounded
# wait for a confirmed group exit before CancelledError propagates.
_CANCEL_TERM_GRACE = 0.5
_CANCEL_KILL_WAIT = 2.0
_ACTIVE_INVENTORY_LIMIT = 100
_COMPLETION_NOTICE_COMMAND_CAP = 1000
_CELL_COMMAND_CAP = 300
# Display MIME the host aggregates into a per-cell summary of the bash()
# commands the cell ran (first capped command, count, line total).
_BASH_COMMAND_MIME = "application/vnd.prime-agent.bash-command+json"
_ASYNCIO_WRAPPER_CALLBACKS = {
    ("asyncio.tasks", "gather.<locals>._done_callback"),
    ("asyncio.tasks", "shield.<locals>._inner_done_callback"),
    ("asyncio.tasks", "_wait.<locals>._on_completion"),
    ("asyncio.tasks", "as_completed.<locals>._on_completion"),
    ("asyncio.tasks", "_release_waiter"),
}

_live_handles: set["BashHandle"] = set()
_live_lock = threading.Lock()
_hook_installed = False
_hook_lock = threading.Lock()


# ---------------------------------------------------------------------------
# Refusals. The guards themselves run in the host; these are the exceptions the
# kernel raises for their verdicts, and the launch-time bypass snapshot it sends.


class DestructiveGitRefusalError(RuntimeError):
    """A destructive git discard was refused on a dirty working tree."""


class DestructiveChmodRefusalError(RuntimeError):
    """A recursive chmod/chown was refused for escaping the workspace."""


class ForcePushRefusalError(RuntimeError):
    """A force-push to a protected branch or the upstream was refused."""


class SecretEchoRefusalError(RuntimeError):
    """A command that would echo secrets into the transcript was refused."""


class PipeToShellRefusalError(RuntimeError):
    """A curl/wget download that a shell interpreter would run was refused."""


class PrivilegeEscalationRefusalError(RuntimeError):
    """Raised when a command would run as root (or another user) via sudo/doas."""


BASH_DESTRUCTIVE_GIT_BYPASS_ENV = "PI_BASH_ALLOW_DESTRUCTIVE_GIT"
BASH_DESTRUCTIVE_CHMOD_BYPASS_ENV = "PI_BASH_ALLOW_DESTRUCTIVE_CHMOD"
BASH_FORCE_PUSH_BYPASS_ENV = "PI_BASH_ALLOW_FORCE_PUSH"
BASH_SECRET_ECHO_BYPASS_ENV = "PI_BASH_ALLOW_SECRET_ECHO"
BASH_PIPE_TO_SHELL_BYPASS_ENV = "PI_BASH_ALLOW_PIPE_TO_SHELL"
BASH_SUDO_BYPASS_ENV = "PI_BASH_ALLOW_SUDO"


GIT_STATUS_PORCELAIN_COMMAND = "git status --porcelain --untracked-files=all"
# How many dirty paths the destructive-git refusal lists before eliding the rest.
MAX_DIRTY_PATHS_LISTED = 10


def is_destructive_git_discard_command(command: str) -> bool:
    """True when `command` contains a git command that discards uncommitted
    working-tree changes (`git checkout -- .`, `git restore .`,
    `git reset --hard`, `git clean` that is not a dry run: the force
    requirement can be turned off in a config the text cannot see)."""
    return bool(_raise_for(_request({"type": "bash.isDestructiveGitDiscard", "command": command})).get("discard"))


def _is_truthy_env_value(value: str | None) -> bool:
    return value is not None and value not in ("", "0")


# The bypass variables are user-launch options, not model-visible switches:
# each is read once here, at kernel start, and frozen, so a cell that writes it
# mid-session cannot silently disarm a guard (the per-call allow_* kwargs are
# the only in-session bypass). A late write only earns a one-time warning.
_BASH_DESTRUCTIVE_GIT_BYPASS_AT_START = _is_truthy_env_value(os.environ.get(BASH_DESTRUCTIVE_GIT_BYPASS_ENV))
_DESTRUCTIVE_CHMOD_BYPASS_AT_KERNEL_START = _is_truthy_env_value(os.environ.get(BASH_DESTRUCTIVE_CHMOD_BYPASS_ENV))
_FORCE_PUSH_BYPASS_AT_KERNEL_START = _is_truthy_env_value(os.environ.get(BASH_FORCE_PUSH_BYPASS_ENV))
_SECRET_ECHO_BYPASS_AT_KERNEL_START = _is_truthy_env_value(os.environ.get(BASH_SECRET_ECHO_BYPASS_ENV))
_PIPE_TO_SHELL_BYPASS_AT_KERNEL_START = _is_truthy_env_value(os.environ.get(BASH_PIPE_TO_SHELL_BYPASS_ENV))
_SUDO_BYPASS_AT_KERNEL_START = _is_truthy_env_value(os.environ.get(BASH_SUDO_BYPASS_ENV))

_destructive_chmod_late_bypass_warned = False
_force_push_late_bypass_warned = False
_secret_echo_late_bypass_warned = False
_pipe_to_shell_late_bypass_warned = False
_sudo_late_bypass_warned = False

# guard key -> (bash() kwarg, frozen launch flag, warn-once flag, refusal class)
_GUARDS: dict[str, tuple[str, str, str | None, type[RuntimeError]]] = {
    "destructive_git": (
        "allow_destructive_git",
        "_BASH_DESTRUCTIVE_GIT_BYPASS_AT_START",
        None,
        DestructiveGitRefusalError,
    ),
    "destructive_chmod": (
        "allow_destructive_chmod",
        "_DESTRUCTIVE_CHMOD_BYPASS_AT_KERNEL_START",
        "_destructive_chmod_late_bypass_warned",
        DestructiveChmodRefusalError,
    ),
    "force_push": (
        "allow_force_push",
        "_FORCE_PUSH_BYPASS_AT_KERNEL_START",
        "_force_push_late_bypass_warned",
        ForcePushRefusalError,
    ),
    "secret_echo": (
        "allow_secret_echo",
        "_SECRET_ECHO_BYPASS_AT_KERNEL_START",
        "_secret_echo_late_bypass_warned",
        SecretEchoRefusalError,
    ),
    "pipe_to_shell": (
        "allow_pipe_to_shell",
        "_PIPE_TO_SHELL_BYPASS_AT_KERNEL_START",
        "_pipe_to_shell_late_bypass_warned",
        PipeToShellRefusalError,
    ),
    "sudo": ("allow_sudo", "_SUDO_BYPASS_AT_KERNEL_START", "_sudo_late_bypass_warned", PrivilegeEscalationRefusalError),
}
_REFUSALS = {error.__name__: (key, error) for key, (_, _, _, error) in _GUARDS.items()}
_REFUSAL_ERRORS = tuple(error for _, _, _, error in _GUARDS.values())


def _launch_bypass() -> list[str]:
    module = sys.modules[__name__]
    return [key for key, (_, flag, _, _) in _GUARDS.items() if getattr(module, flag)]


# ---------------------------------------------------------------------------
# Transport: kernel host requests inside a Prime Agent kernel, the sidecar
# everywhere else.

# The host exports this to the kernels whose bash() requests it serves; read
# once at import, like the bypass snapshot.
_HOST_SERVES_BASH = os.environ.get("PRIME_AGENT_HOST_BASH") == "1"
_SIDECAR_FLAG = "--prime-agent-bash-host"


class BashHostUnavailable(RuntimeError):
    """The bash host (the kernel's Prime Agent host, or the sidecar) cannot answer."""


# Why a sidecar can vanish at once: a binary that predates the flag treats
# it as an ordinary run and exits.
_SIDECAR_SKEW_HINT = (
    f"a prime-agent binary without {_SIDECAR_FLAG} is older than this prime-agent-runtime "
    "(host/runtime version skew). Reinstall prime-agent so the binary and its runtime match "
    "(`cargo install --path crates/pa-cli` from the checkout, or rerun the installer), or set "
    "PRIME_AGENT_EXECUTABLE to a current prime-agent binary"
)


def _raise_for(reply: dict[str, Any]) -> dict[str, Any]:
    """`reply` when it succeeded, else the exception it names."""
    status = reply.get("status")
    if status == "ok":
        return reply
    message = str(reply.get("message") or "")
    if status == "refused":
        key, error = _REFUSALS.get(str(reply.get("error")), ("", RuntimeError))
        _warn_once(key, reply.get("warning"))
        raise error(message)
    name = reply.get("error")
    if name == "OSError":
        errno = reply.get("errno")
        if isinstance(errno, int):
            raise OSError(errno, os.strerror(errno))
        raise OSError(message)
    if name == "KeyError":
        raise KeyError(message)
    errors: dict[str, type[Exception]] = {"ValueError": ValueError, "TypeError": TypeError}
    raise errors.get(str(name), RuntimeError)(message or f"bash host request failed: {reply!r}")


def _warn_once(key: str, warning: Any) -> None:
    """Print a guard's late-bypass warning at most once per kernel."""
    flag = _GUARDS.get(key, (None, None, None, None))[2]
    if flag is None or not isinstance(warning, str) or not warning:
        return
    module = sys.modules[__name__]
    if getattr(module, flag):
        return
    setattr(module, flag, True)
    print(warning, file=sys.stderr, flush=True)


class _Slot:
    """One pending sidecar request's settlement."""

    def __init__(self, loop: asyncio.AbstractEventLoop | None = None) -> None:
        self.done = threading.Event()
        self.data: dict[str, Any] | None = None
        self.error: BaseException | None = None
        self.loop = loop
        self.future: asyncio.Future[dict[str, Any]] | None = loop.create_future() if loop else None
        # The sidecar process the request went to (its death fails the slot).
        self.proc: subprocess.Popen[bytes] | None = None

    def settle(self, data: dict[str, Any] | None, error: BaseException | None) -> None:
        self.data, self.error = data, error
        self.done.set()
        future, loop = self.future, self.loop
        if future is None or loop is None:
            return

        def deliver() -> None:
            if future.done():
                return
            if error is not None:
                future.set_exception(error)
            else:
                future.set_result(data or {})

        try:
            loop.call_soon_threadsafe(deliver)
        except RuntimeError:
            pass  # the awaiting loop already closed


def _checkout_host() -> Path | None:
    """The host binary of the source checkout this runtime runs from, if built."""
    name = "prime-agent.exe" if os.name == "nt" else "prime-agent"
    checkout = Path(__file__).resolve().parents[3] / "target" / "debug" / name
    return checkout if checkout.is_file() else None


def _host_executable() -> str:
    configured = (os.environ.get("PRIME_AGENT_EXECUTABLE") or "").strip()
    if configured:
        return configured
    checkout = _checkout_host()
    if checkout is not None:
        return str(checkout)
    raise BashHostUnavailable(
        "rlm.bash needs the Prime Agent host: call it inside a Prime Agent kernel, "
        "or set PRIME_AGENT_EXECUTABLE to the prime-agent binary"
    )


class _Sidecar:
    """`prime-agent --prime-agent-bash-host` as this process's bash host: one
    JSON line per request and reply. Its jobs die with it, and it exits (killing
    them) when this process does, since its stdin closes."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._write_lock = threading.Lock()
        self._proc: subprocess.Popen[bytes] | None = None
        self._pending: dict[str, _Slot] = {}
        self._executable = ""

    def _gone(self, what: str) -> BashHostUnavailable:
        return BashHostUnavailable(f"the bash host ({self._executable}) {what}; {_SIDECAR_SKEW_HINT}")

    def ensure_started(self) -> subprocess.Popen[bytes]:
        with self._lock:
            if self._proc is not None and self._proc.poll() is None:
                return self._proc
            self._executable = _host_executable()
            proc = subprocess.Popen(
                [self._executable, _SIDECAR_FLAG],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
            self._proc = proc
            threading.Thread(target=self._read, args=(proc,), daemon=True).start()
            return proc

    def _read(self, proc: subprocess.Popen[bytes]) -> None:
        assert proc.stdout is not None
        for line in proc.stdout:
            try:
                frame = json.loads(line)
            except ValueError:
                continue
            with self._lock:
                slot = self._pending.pop(str(frame.get("id")), None)
            data = frame.get("data")
            if slot is not None:
                slot.settle(data if isinstance(data, dict) else {}, None)
        with self._lock:
            if self._proc is proc:
                self._proc = None
            stranded = [slot for slot in self._pending.values() if slot.proc is proc]
            self._pending = {key: slot for key, slot in self._pending.items() if slot.proc is not proc}
        for slot in stranded:
            slot.settle(None, self._gone("exited before answering"))

    def _send(self, data: dict[str, Any], slot: _Slot) -> str:
        proc = self.ensure_started()
        rid = uuid.uuid4().hex
        with self._lock:
            # The reader clears `_proc` once the host exits: a host that exited
            # between its start and this request is the "exited before
            # answering" failure (with the skew hint), whichever thread noticed.
            if self._proc is not proc or proc.stdin is None:
                raise self._gone("exited before answering")
            slot.proc = proc
            self._pending[rid] = slot
        line = (json.dumps({"id": rid, "data": data}, separators=(",", ":")) + "\n").encode()
        try:
            with self._write_lock:
                proc.stdin.write(line)
                proc.stdin.flush()
        except OSError as err:
            with self._lock:
                self._pending.pop(rid, None)
            # A host that exited at once is the same failure as one that
            # exited before answering, whichever the write noticed first.
            raise self._gone("exited before answering") from err
        return rid

    def _cancel(self, rid: str, slot: _Slot) -> None:
        line = (json.dumps({"id": rid, "cancel": True}, separators=(",", ":")) + "\n").encode()
        proc = slot.proc
        if proc is None or proc.stdin is None:
            return
        try:
            with self._write_lock:
                _ = proc.stdin.write(line)
                proc.stdin.flush()
        except OSError:
            pass  # the sidecar is gone, and its jobs with it

    def request(self, data: dict[str, Any], *, cancel_on_interrupt: bool = False) -> dict[str, Any]:
        slot = _Slot()
        rid = self._send(data, slot)
        try:
            _ = slot.done.wait()
        except KeyboardInterrupt as interrupt:
            if not cancel_on_interrupt:
                raise
            self._cancel(rid, slot)
            _drain(slot.done, _CANCEL_DRAIN)
            interrupt.bash_reply = slot.data if slot.error is None else None  # pyright: ignore[reportAttributeAccessIssue]
            raise
        if slot.error is not None:
            raise slot.error
        return slot.data or {}

    async def arequest(self, data: dict[str, Any]) -> dict[str, Any]:
        slot = _Slot(asyncio.get_running_loop())
        _ = self._send(data, slot)
        assert slot.future is not None
        return await slot.future


_sidecar = _Sidecar()

# How long an interrupted `bash.run` waits for its host to kill the job and
# answer (the teardown itself is bounded at about 2.5 s host-side).
_CANCEL_DRAIN = 10.0


def _drain(done: threading.Event, timeout: float) -> None:
    """Wait (bounded) for a cancelled request's reply; repeat interrupts are
    consumed, as the async cancel drain does."""
    deadline = time.monotonic() + timeout
    while not done.is_set():
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return
        try:
            _ = done.wait(remaining)
        except KeyboardInterrupt:
            continue


def _host_mode() -> bool:
    if not _HOST_SERVES_BASH:
        return False
    from . import repl

    return repl.is_active()


def _unwrap_host(reply: dict[str, Any]) -> dict[str, Any]:
    if reply.get("status") != "ok":
        from . import repl

        error = str(reply.get("error") or "bash host request failed")
        if repl.UNSERVED_HOST_REQUEST in error:
            raise BashHostUnavailable(f"{error}: {repl.host_skew_message('this bash() request')}")
        raise BashHostUnavailable(error)
    result = reply.get("result")
    return result if isinstance(result, dict) else {}


def _request(data: dict[str, Any], *, cancel_on_interrupt: bool = False) -> dict[str, Any]:
    """Send one `bash.*` request and block for its reply (any thread).

    With ``cancel_on_interrupt``, a KeyboardInterrupt during the wait cancels
    the request host-side (a `bash.run` kills its job), waits for the reply,
    and re-raises with that reply (or None) as its ``bash_reply``.
    """
    reply = _request_once(data, cancel_on_interrupt=cancel_on_interrupt)
    if reply.get("status") == "error" and reply.get("error") == "EnvUnknown":
        # The host (a restarted sidecar, or a newer environment from another
        # thread) does not hold the environment this key names.
        resend = _env_resend(data)
        if resend is not None:
            return _request_once(resend, cancel_on_interrupt=cancel_on_interrupt)
    return reply


def _request_once(data: dict[str, Any], *, cancel_on_interrupt: bool) -> dict[str, Any]:
    if _host_mode():
        from . import repl

        try:
            reply = (
                repl.host_request_blocking(data, cancel_on_interrupt=True, drain_timeout_s=_CANCEL_DRAIN)
                if cancel_on_interrupt
                else repl.host_request_blocking(data)
            )
        except KeyboardInterrupt as interrupt:
            host_reply = getattr(interrupt, "host_reply", None)
            interrupt.bash_reply = (  # pyright: ignore[reportAttributeAccessIssue]
                _unwrap_host(host_reply) if isinstance(host_reply, dict) and host_reply.get("status") == "ok" else None
            )
            raise
        return _unwrap_host(reply)
    if cancel_on_interrupt:
        return _sidecar.request(data, cancel_on_interrupt=True)
    return _sidecar.request(data)


async def _arequest(data: dict[str, Any]) -> dict[str, Any]:
    if _host_mode():
        from . import repl

        return _unwrap_host(await repl.host_request(data))
    return await _sidecar.arequest(data)


def _event_output(event: dict[str, Any], job_id: str) -> str:
    """A finished event's result text: inline, or (a long one) in the spill
    file the host wrote to this kernel's temp directory, read and removed.
    A spill file that cannot be read falls back to the host's buffer."""
    path = event.get("outputFile")
    if not isinstance(path, str):
        return str(event.get("output", ""))
    try:
        with open(path, encoding="utf-8", newline="") as spilled:
            return spilled.read()
    except OSError:
        return str(_raise_for(_request({"type": "bash.output", "id": job_id})).get("output", ""))
    finally:
        with contextlib.suppress(OSError):
            os.unlink(path)


# The kernel environment last sent whole, and the key the host remembers it
# under: an unchanged environment travels as its key alone.
_env_lock = threading.Lock()
_env_sent: tuple[str, dict[str, str]] | None = None


def _env_fields() -> dict[str, Any]:
    """`os.environ` right now, as the request fields that carry it."""
    global _env_sent
    env = dict(os.environ)
    with _env_lock:
        if _env_sent is not None and _env_sent[1] == env:
            return {"envKey": _env_sent[0]}
        _env_sent = (uuid.uuid4().hex, env)
        return {"envKey": _env_sent[0], "env": env}


def _env_resend(data: dict[str, Any]) -> dict[str, Any] | None:
    """`data` with its environment whole, for a host that lost the key: the
    environment the key named, which the host remembers again under it (or,
    when another thread has replaced it since, the current one, unkeyed)."""
    key = data.get("envKey")
    if not isinstance(key, str) or "env" in data:
        return None
    with _env_lock:
        sent = _env_sent
    if sent is not None and sent[0] == key:
        return {**data, "env": sent[1]}
    resend = {name: value for name, value in data.items() if name != "envKey"}
    resend["env"] = dict(os.environ)
    return resend


def _kernel_request(kind: str, command: str, script: str, command_prefix: str | None, **extra: Any) -> dict[str, Any]:
    """The kernel state every check and spawn carries: its cwd and environment
    right now, the launch-time bypasses, and the current trace context."""
    data: dict[str, Any] = {
        "type": kind,
        "command": command,
        "script": script,
        "cwd": os.getcwd(),
        **_env_fields(),
        "launchBypass": _launch_bypass(),
        "kernelPid": os.getpid(),
    }
    if command_prefix is not None:
        data["prefix"] = command_prefix
    data.update(extra)
    return data


# ---------------------------------------------------------------------------
# REPL cell lifecycle


def _current_cell_completion_context() -> tuple[asyncio.Event, asyncio.Task[Any] | None] | None:
    """Get the creating REPL cell's lifecycle without coupling standalone use to repl."""
    try:
        from . import repl

        if repl.is_active():
            return repl.current_cell_completion_context()
    except (ImportError, RuntimeError):
        pass
    return None


def _current_cell_bash_recorder() -> Callable[[dict[str, Any]], None] | None:
    """The creating cell's command log, or None outside a REPL cell."""
    try:
        from . import repl

        if repl.is_active():
            return repl.current_cell_bash_recorder()
    except (ImportError, RuntimeError):
        pass
    return None


def _consume_notice_task(task: asyncio.Task[None]) -> None:
    """Retrieve detached notifier failures so they never become loop warnings."""
    if not task.cancelled():
        task.exception()


def _capped(command: str) -> str:
    if len(command) > _COMPLETION_NOTICE_COMMAND_CAP:
        return command[:_COMPLETION_NOTICE_COMMAND_CAP] + "\n... [command truncated]"
    return command


def _completion_reaches(start: asyncio.Future[Any], targets: tuple[asyncio.Future[Any], ...]) -> bool:
    """Follow asyncio's wrapper and TaskGroup ownership callbacks."""
    pending = [start]
    seen_futures: set[int] = set()
    seen_values: set[int] = set()

    def collect(value: Any, depth: int = 0) -> None:
        if isinstance(value, asyncio.Future):
            pending.append(value)
            return
        identity = id(value)
        if depth >= 4 or identity in seen_values:
            return
        seen_values.add(identity)

        nested: list[Any] = []
        if isinstance(value, asyncio.Queue):
            pending.extend(value._getters)  # pyright: ignore[reportAttributeAccessIssue, reportUnknownMemberType, reportUnknownArgumentType]
        elif isinstance(value, functools.partial):
            nested.extend((value.func, value.args, value.keywords))
        elif isinstance(value, dict):
            nested.extend(value.keys())
            nested.extend(value.values())
        elif isinstance(value, (tuple, list, set, frozenset)):
            nested.extend(value)
        else:
            closure = getattr(value, "__closure__", None) or ()
            for cell in closure:
                try:
                    nested.append(cell.cell_contents)
                except ValueError:
                    pass
            bound_self = getattr(value, "__self__", None)
            if bound_self is not None:
                nested.append(bound_self)
        for item in nested:
            collect(item, depth + 1)

    while pending:
        future = pending.pop()
        if any(future is target for target in targets):
            return True
        if id(future) in seen_futures:
            continue
        seen_futures.add(id(future))
        for entry in getattr(future, "_callbacks", None) or ():
            callback = entry[0] if isinstance(entry, tuple) else entry
            base = callback.func if isinstance(callback, functools.partial) else callback
            identity = (getattr(base, "__module__", None), getattr(base, "__qualname__", None))
            if identity in _ASYNCIO_WRAPPER_CALLBACKS:
                collect(callback)
            elif identity == ("asyncio.tasks", "_AsCompletedIterator._handle_completion"):
                collect(base.__self__._done)  # pyright: ignore
            elif identity == (None, "Task.task_wakeup"):
                task = getattr(callback, "__self__", None)
                if isinstance(task, asyncio.Task):
                    pending.append(task)
            elif identity == ("asyncio.taskgroups", "TaskGroup._on_task_done"):
                parent = getattr(getattr(callback, "__self__", None), "_parent_task", None)
                if isinstance(parent, asyncio.Future):
                    pending.append(parent)
    return False


def _creating_cell_waits_for(owner: asyncio.Task[Any] | None, awaiter: asyncio.Task[Any] | None) -> bool:
    """Return whether the cell owner directly or transitively waits for awaiter."""
    if owner is None or awaiter is None:
        return False
    if owner is awaiter:
        return True
    waiter = getattr(owner, "_fut_waiter", None)
    targets: tuple[asyncio.Future[Any], ...] = (owner,)
    if isinstance(waiter, asyncio.Future):
        targets += (waiter,)
    return _completion_reaches(awaiter, targets)


def _live_cell_owner() -> asyncio.Task[Any] | None:
    """Body task of the cell executing right now, ignoring detached context copies."""
    try:
        from . import repl

        if repl.is_active():
            return repl.active_cell_task()
    except (ImportError, RuntimeError):
        pass
    return None


# `bash.run` follows a command it just started for this long, so a quick
# command's whole life (result and reap) is one host request. While another
# command is in flight the window is skipped: `bash()` blocks its caller for
# the window, and a fan-out of long commands must not start one window apart.
# A handle whose result is in no longer counts, though it stays live until its
# reap arrives (at once, or when a lingering background group exits): an
# awaited command followed by the next must not race that reap.
_RUN_WINDOW_MS = 25


@dataclass
class _Launch:
    """One `bash.run`: the span the command carries, and what the host
    answered (the job and its events so far), or why it did not start."""

    command: str
    script: str
    span: trace.Span
    started: float
    reply: dict[str, Any] | None = None
    error: BaseException | None = None
    interrupt: KeyboardInterrupt | None = None


# bash() hands its launch to the BashHandle it builds next on this thread.
_handoff = threading.local()


def _launch(command: str, script: str, command_prefix: str | None, allow: list[str] | None) -> _Launch:
    """Check, spawn and briefly follow `script` in one host request.

    ``allow`` lists the bypassed guards; None skips the guards (a caller that
    already validated the script). A guard refusal raises here; plan mode's
    refusal and a spawn failure come back as the launch's ``error``, raised by
    the handle (which ends the command's span with it). An interrupt during
    the wait kills the job host-side and comes back as ``interrupt``.
    """
    span = trace.Span(
        name="bash.command", ctx=trace.child_context(trace.current()), attrs={"bash.command": _safe_command(command)}
    )
    launch = _Launch(command=command, script=script, span=span, started=time.monotonic())
    # Plan mode's in-kernel fallback guard (no OS sandbox on this machine)
    # refuses before any process exists; under the OS sandbox the host runs
    # the command inside it, read-only. A kernel guard refusal still wins: it
    # is what the command met first before the two checks shared one request.
    try:
        plan_guard.check_bash(script)
    except BaseException as error:  # noqa: BLE001 - re-raised by the handle
        if allow is not None:
            _run_kernel_bash_guards(command, script, command_prefix, **{_GUARDS[key][0]: True for key in allow})
        launch.error = error
        return launch
    with _live_lock:
        in_flight = any(not handle._done.is_set() for handle in _live_handles)
    window = 0 if in_flight else _RUN_WINDOW_MS
    data = _kernel_request(
        "bash.run",
        command,
        script,
        command_prefix,
        traceparent=trace.format_traceparent(span.ctx),
        waitMs=window,
        spillDir=tempfile.gettempdir(),
    )
    if allow is None:
        data["guards"] = False
    else:
        data["allow"] = allow
        if (ctx := trace.current()) is not None:
            data["checkTraceparent"] = trace.format_traceparent(ctx)
    try:
        reply = _request(data, cancel_on_interrupt=True)
    except KeyboardInterrupt as interrupt:
        launch.interrupt = interrupt
        reply = getattr(interrupt, "bash_reply", None)
        if not isinstance(reply, dict) or not isinstance(reply.get("job"), dict):
            raise
        launch.reply = reply
        return launch
    try:
        launch.reply = _raise_for(reply)
    except _REFUSAL_ERRORS:
        raise
    except Exception as error:  # noqa: BLE001 - re-raised by the handle
        launch.error = error
    return launch


@dataclass(frozen=True)
class BashResult:
    exit_code: int
    output: str
    duration: float

    def __await__(self) -> Generator[Any, None, BashResult]:
        # `h = await bash(cmd)` followed by `await h` is a common slip: awaiting
        # a result just gives it back, without suspending.
        yield from ()
        return self


_OUTPUT_HINT = (
    "BashHandle.output is a method: call h.output() for the text so far, "
    "or use (await h).output for the finished result"
)


@final
class _BoundOutput:
    """`handle.output`, the bound method: calling it reads the text; using it
    as the text (a subscript, `len()`, a `str` method) names both spellings
    instead of failing as an opaque method object."""

    __slots__ = ("_handle", "_read")

    def __init__(self, handle: BashHandle, read: Callable[[BashHandle], str]) -> None:
        self._handle: BashHandle = handle
        self._read: Callable[[BashHandle], str] = read

    def __call__(self) -> str:
        return self._read(self._handle)

    def __getitem__(self, _key: object) -> NoReturn:
        raise TypeError(f"'method' object is not subscriptable; {_OUTPUT_HINT}")

    def __len__(self) -> NoReturn:
        raise TypeError(f"object of type 'method' has no len(); {_OUTPUT_HINT}")

    def __iter__(self) -> NoReturn:
        raise TypeError(f"'method' object is not iterable; {_OUTPUT_HINT}")

    def __contains__(self, _item: object) -> NoReturn:
        raise TypeError(f"argument of type 'method' is not iterable; {_OUTPUT_HINT}")

    def __getattr__(self, name: str) -> NoReturn:
        if hasattr(str, name):
            raise AttributeError(f"'method' object has no attribute {name!r}; {_OUTPUT_HINT}")
        raise AttributeError(f"'method' object has no attribute {name!r}")

    def __repr__(self) -> str:
        return f"<bound method BashHandle.output of {self._handle!r}>"


@final
class _OutputMethod:
    """The `output` descriptor: the plain function on the class (so
    `BashHandle.output` stays callable), a `_BoundOutput` on a handle."""

    def __init__(self, read: Callable[[BashHandle], str]) -> None:
        self._read: Callable[[BashHandle], str] = read
        self.__doc__ = read.__doc__

    @overload
    def __get__(self, handle: None, owner: type | None = None) -> Callable[[BashHandle], str]: ...
    @overload
    def __get__(self, handle: BashHandle, owner: type | None = None) -> _BoundOutput: ...
    def __get__(
        self, handle: BashHandle | None, owner: type | None = None
    ) -> Callable[[BashHandle], str] | _BoundOutput:
        if handle is None:
            return self._read
        return _BoundOutput(handle, self._read)


class BashHandle:
    """Live handle to a shell command; await it for the BashResult.

    A handle awaited before any other API use (the `await bash(cmd)` one-shot
    form, including `h = bash(cmd)` awaited immediately) owns the command:
    cancelling that await kills the process group. Touching .pid/.running/
    .output()/.tail()/.poll()/.kill() first marks the handle as a background
    handle; later awaits only wait and cancelling them leaves it running.
    """

    def __init__(self, command: str, script: str | None = None, _validated: bool = False) -> None:
        # `command` is the text the caller wrote and stays the display value
        # (the completion notice and repr use it). `script` is the text the
        # shell runs, computed once by `bash()` from a single read of
        # PRIME_AGENT_BASH_COMMAND_PREFIX and launched (checked and spawned in
        # one host request) there; a handle built directly is guarded here on
        # the same one read that supplies its script, so constructing the
        # class is not a way around the guards.
        handed_over = False
        if script is None:
            command_prefix = os.environ.get("PRIME_AGENT_BASH_COMMAND_PREFIX")
            script = _prefix_command(command, command_prefix)
            launch = _launch(command, script, command_prefix, [])
        elif not _validated:
            # A caller-supplied script has no trusted prefix region: scan the
            # whole text as user text so a prefix boundary cannot hide words.
            launch = _launch(command, script, None, [])
        else:
            pending: _Launch | None = getattr(_handoff, "launch", None)
            _handoff.launch = None
            if pending is not None and pending.command == command and pending.script == script:
                launch = pending
                handed_over = True
            else:
                launch = _launch(command, script, None, None)
        self.command = command
        # One "bash.command" span per call, a child of the calling cell's
        # context (or a fresh trace); the child process inherits it through
        # TRACEPARENT. _end_span finishes it exactly once from whichever path
        # observes completion first.
        self._span = launch.span
        self._span_lock = threading.Lock()
        self._span_context = contextvars.copy_context()
        self._killed = False
        self._script = script
        completion_context = _current_cell_completion_context()
        self._creating_cell_finished = completion_context[0] if completion_context else None
        self._creating_cell_task = completion_context[1] if completion_context else None
        self._awaited_by_creating_cell = False
        self._cell_bash_recorder = _current_cell_bash_recorder()
        self._started = launch.started
        self._fields: dict[str, Any] = {}
        self._done = threading.Event()
        self._reaped = False
        self._result: BashResult | None = None
        self._final_output: str | None = None
        self._callbacks: list[Callable[[], None]] = []
        self._reap_callback: Callable[[], None] | None = None
        self._result_consumed = False
        self._consumed_notice: Callable[[], None] | None = None
        self._callback_lock = threading.Lock()
        self._released = False
        # The host's job, filled in from the launch before the handle is returned.
        self._activity_id = ""
        self._pid = 0
        self._pgid = 0
        self._started_at_text = ""
        self._started_at = datetime.now(timezone.utc)
        try:
            self._adopt(launch)
        except BaseException as exc:
            self._end_span(error=_truncate(f"spawn failed: {type(exc).__name__}: {exc}"))
            raise
        if launch.interrupt is not None and not handed_over:
            raise launch.interrupt

    def _adopt(self, launch: _Launch) -> None:
        """Take over the launched job: its events so far settle the handle
        (a quick command arrives finished and reaped), and a follower thread
        mirrors the rest."""
        if launch.error is not None:
            raise launch.error
        reply = launch.reply or {}
        job = reply["job"]
        self._activity_id = str(job["id"])
        self._pid = int(job["pid"])
        self._pgid = int(job["pgid"])
        self._started_at_text = str(job["startedAt"])
        self._started_at = datetime.fromisoformat(self._started_at_text)
        self._span.attrs.update(
            {"bash.pid": self._pid, "bash.pgid": self._pgid, "bash.started_at": self._started_at_text}
        )
        self._span.emit_start()
        if launch.interrupt is not None:
            # The interrupted run killed the command it owned, like a
            # cancelled one-shot await: no completion notice follows.
            self._killed = True
            self._awaited_by_creating_cell = True
        with _live_lock:
            _live_handles.add(self)
        for event in reply.get("events", ()):
            self._apply(event)
        if not reply.get("done"):
            threading.Thread(target=self._follow, args=(int(reply.get("cursor", 0)),), daemon=True).start()
        self._schedule_background_completion_notice()

    @property
    def pid(self) -> int:
        self._released = True
        return self._pid

    @property
    def running(self) -> bool:
        # Group liveness, matching kill()'s guard and the journal; poll()/await
        # keep foreground result semantics after `cmd &` returns early.
        self._released = True
        return not self._reaped

    def _output_text(self) -> str:
        if self._final_output is not None:
            return self._final_output
        return str(_raise_for(_request({"type": "bash.output", "id": self._activity_id})).get("output", ""))

    def _read_output(self) -> str:
        """The output text so far (the whole text once the job has ended)."""
        self._released = True
        self._note_result_consumed()
        return self._output_text()

    output: _OutputMethod = _OutputMethod(_read_output)

    def peek_output(self) -> str:
        """The current output text without marking the result consumed.

        Reading through `output()`/`tail()` consumes the result (the
        completion notice withdraws), so quiet watchers — `rlm.watch.job`
        polling growth byte ranges — read through this accessor instead
        and leave the job's normal completion notice intact.
        """
        self._released = True
        return self._output_text()

    def peek_output_bytes(self) -> int:
        """The job's stream byte offset without marking the result consumed.

        The rendered `peek_output()` text stops growing once the bounded
        buffer trims (the drop marker replaces real output), so watch
        notices report ranges over THIS offset: it counts every byte the
        process stream produced, past the buffer caps included.
        """
        self._released = True
        if self._reaped:
            return int(self._fields.get("bash.output_bytes", 0))
        reply = _raise_for(_request({"type": "bash.output", "id": self._activity_id, "bytesOnly": True}))
        return int(reply.get("bytes", 0))

    def tail(self, n: int = 50) -> str:
        self._released = True
        self._note_result_consumed()
        return "\n".join(self._output_text().splitlines()[-n:])

    def poll(self) -> BashResult | None:
        self._released = True
        self._note_result_consumed()
        return self._result if self._done.is_set() else None

    def kill(self, sig: int = signal.SIGTERM, grace: float = 5.0) -> None:
        # Guard on group death, not _done: kill() must still reach a lingering
        # background group after the foreground result was already delivered.
        self._released = True
        self._killed = True
        if self._reaped:
            return
        _request({"type": "bash.kill", "id": self._activity_id, "signal": int(sig), "graceMs": int(grace * 1000)})

    def _follow(self, cursor: int = 0) -> None:
        """Mirror the host's job events: progress events become trace events,
        the result finalizes the handle, and the reap releases it."""
        try:
            while True:
                reply = _raise_for(
                    _request(
                        {
                            "type": "bash.follow",
                            "id": self._activity_id,
                            "cursor": cursor,
                            "spillDir": tempfile.gettempdir(),
                        }
                    )
                )
                for event in reply.get("events", ()):
                    self._apply(event)
                cursor = int(reply.get("cursor", cursor))
                if reply.get("done"):
                    return
        except BaseException as exc:  # noqa: BLE001 - the follower must always release the handle
            # The host is gone (kernel teardown, a dead sidecar): the command
            # died with it, so settle the handle as killed rather than leave
            # awaiters hanging.
            self._finalize(-signal.SIGKILL, "", time.monotonic() - self._started)
            self._release(None)
            if not isinstance(exc, (BashHostUnavailable, OSError, RuntimeError, KeyError)):
                raise

    def _apply(self, event: dict[str, Any]) -> None:
        kind = event.get("type")
        if kind == "progress":
            fields = event.get("fields") or {}
            self._fields.update(fields)
            self._emit_progress(str(event.get("msg")), fields)
        elif kind == "finished":
            fields = event.get("fields") or {}
            self._fields.update(fields)
            self._finalize(
                int(event["exitCode"]), _event_output(event, self._activity_id), float(event.get("duration", 0.0))
            )
        elif kind == "reaped":
            self._release(event)

    def _release(self, event: dict[str, Any] | None) -> None:
        if self._final_output is None:
            result = self._result
            if event is not None and int(event.get("bytes", -1)) != int(self._fields.get("bash.output_bytes", -2)):
                # Output after the fence (an EXIT trap, a background job): keep
                # the whole final text, the host evicts finished jobs.
                try:
                    self._final_output = str(
                        _raise_for(_request({"type": "bash.output", "id": self._activity_id})).get("output", "")
                    )
                except BaseException:  # noqa: BLE001 - fall back to the result text
                    self._final_output = result.output if result else ""
            else:
                self._final_output = result.output if result else ""
        with self._callback_lock:
            self._reaped = True
            callback, self._reap_callback = self._reap_callback, None
        if callback is not None:
            callback()
        with _live_lock:
            _live_handles.discard(self)

    def _emit_progress(self, msg: str, fields: dict[str, Any]) -> None:
        event: dict[str, Any] = {"traceId": self._span.trace_id, "spanId": self._span.span_id, **fields}
        if self._span.parent_span_id is not None:
            event["parentSpanId"] = self._span.parent_span_id
        trace.emit_event("bash", msg, **event)

    def _finalize(self, exit_code: int, output: str, duration: float) -> None:
        with self._callback_lock:
            if self._done.is_set():
                return
            self._result = BashResult(exit_code=exit_code, output=output, duration=duration)
            self._done.set()
            callbacks = self._callbacks
            self._callbacks = []
        # Emit before waking awaiters so the span precedes any work that follows the result.
        self._end_span(exit_code=exit_code)
        # Before the awaiters wake too, so an awaiting cell always finds its command recorded.
        self._record_in_cell(exit_code)
        for callback in callbacks:
            callback()

    def _record_in_cell(self, exit_code: int) -> None:
        """Report the finished command to the creating cell, which keeps it only while its body runs."""
        recorder = self._cell_bash_recorder
        if recorder is None:
            return
        try:
            command = _redact_command(self.command)
            record: dict[str, Any] = {
                "command": command[:_CELL_COMMAND_CAP],
                "exitCode": exit_code,
                "startedAt": self._started_at.isoformat(),
                "endedAt": datetime.now(timezone.utc).isoformat(),
            }
            if len(command) > _CELL_COMMAND_CAP:
                record["commandTruncated"] = True
            recorder(record)
        except BaseException:  # noqa: BLE001 - awaiters wake after this; it must never raise
            return

    def _end_span(self, exit_code: int | None = None, error: str | None = None) -> None:
        """Finish the bash.command span exactly once; never raises.

        Called from _finalize (result known), from a failed spawn, and from
        _kill_live_handles (kernel shutdown with the command still running:
        the span ends as ``error`` "kernel shutdown" rather than dangling
        unfinished in the trace). Later calls are no-ops, so the shutdown end
        and a racing _finalize from the follower thread cannot double-emit.
        """
        try:
            with self._span_lock:
                span = self._span
                if span.ended:
                    return
                attrs = span.attrs
                if exit_code is not None:
                    attrs["bash.exit_code"] = exit_code
                    if exit_code < 0:
                        # Death by signal reports as -signum (POSIX).
                        attrs["bash.signal"] = _signal_name(-exit_code)
                    if error is None and exit_code != 0:
                        error = f"killed by {attrs['bash.signal']}" if exit_code < 0 else f"exit code {exit_code}"
                if self._killed:
                    attrs["bash.killed"] = True
                now = time.monotonic()
                attrs.update(self._fields)
                if self._started_at_text:
                    attrs["bash.started_at"] = self._started_at_text
                attrs["bash.elapsed_ms"] = round((now - self._started) * 1000)
                attrs.setdefault("bash.output_bytes", 0)
                attrs.setdefault("bash.silence_ms", round((now - self._started) * 1000))
                self._span_context.copy().run(span.end, error=error)
        except BaseException:  # noqa: BLE001 - tracing must never break the traced command
            return

    def _add_done_callback(self, callback: Callable[[], None]) -> None:
        with self._callback_lock:
            if not self._done.is_set():
                self._callbacks.append(callback)
                return
        callback()

    def _note_result_consumed(self, awaiter: asyncio.Task[Any] | None = None) -> None:
        """Record a result read that reaches the model: only reads during a live
        cell count (a detached reader between turns must keep the notice — it is
        the idle session's only wake-up), and an awaiting reader must be one the
        live cell waits for."""
        if not self._done.is_set():
            return
        owner = _live_cell_owner()
        if owner is None:
            return
        if awaiter is not None and not _creating_cell_waits_for(owner, awaiter):
            return
        with self._callback_lock:
            if self._result_consumed:
                return
            self._result_consumed = True
            notice, self._consumed_notice = self._consumed_notice, None
        if notice is not None:
            notice()

    def _schedule_background_completion_notice(self) -> None:
        cell_finished = self._creating_cell_finished
        if cell_finished is None:
            return
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return
        from . import repl

        activity = {"id": self._activity_id, "pid": self._pid, "active": True}
        # Publish synchronously before bash() returns and the creating cell can end.
        repl.emit({"application/vnd.prime-agent.bash-activity+json": activity})
        notice = self._notify_background_completion(cell_finished, activity)
        try:
            task = loop.create_task(notice)
        except BaseException:
            self.kill(signal.SIGKILL if _IS_POSIX else signal.SIGTERM)
            notice.close()
            repl.emit({"application/vnd.prime-agent.bash-activity+json": {**activity, "active": False}})
            raise
        task.add_done_callback(_consume_notice_task)

    async def _notify_background_completion(self, cell_finished: asyncio.Event, activity: dict[str, Any]) -> None:
        from . import repl

        try:
            result = await self._wait()
            await self._wait_reaped()
            # The cell may do other work before awaiting this handle. Do not classify
            # it as detached until that whole cell has crossed its completion barrier.
            await cell_finished.wait()
            if self._awaited_by_creating_cell or self._result_consumed or not repl.is_active():
                return
            command = _capped(self.command)
            reply = await repl.host_request(
                {
                    "type": "bash.completed",
                    "pid": self._pid,
                    "command": command,
                    "exitCode": result.exit_code,
                }
            )
            if isinstance(reply, dict) and reply.get("status") == "ok":
                # Notice accepted by the host; later reads must ask it to withdraw.
                self._arm_consumed_notice(command)
            else:
                sys.stderr.write(
                    f"Background bash completion follow-up for pid {self._pid} was not accepted. "
                    "Inspect the saved handle with poll(), output(), or tail().\n"
                )
        except (OSError, RuntimeError):
            # Standalone runtimes have no host handler, and teardown can close
            # the bridge while a process is finishing. Shell results stay usable.
            return
        finally:
            # Reap and deliver (or report rejection) before releasing kernel residency.
            repl.emit({"application/vnd.prime-agent.bash-activity+json": {**activity, "active": False}})

    def _arm_consumed_notice(self, command: str) -> None:
        # Armed only post-acceptance: the withdrawal can never overtake its notice.
        dispatch = functools.partial(self._notify_result_consumed, command)

        with self._callback_lock:
            if not self._result_consumed:
                self._consumed_notice = dispatch
                return
        dispatch()

    def _notify_result_consumed(self, command: str) -> None:
        """Ship the withdrawal inside the read, ahead of the cell's done event.

        The host delivers a queued notice at the reading cell's turn boundary,
        which begins when that cell's done event is processed: a withdrawal
        frame that leaves the kernel after done arrives too late, and the stale
        notice wakes the model anyway. Reads happen inside a live cell, so
        writing the frame right here puts it ahead of done on the wire, where
        the host must withdraw before it can dispatch. The reply never matters
        (unknown reply ids are dropped), so the request is fire-and-forget:
        no future to await, no event-loop hop that could run after the cell.
        """
        from . import repl

        if not repl.is_active():
            return
        repl._send(
            {
                "event": "host_request",
                "id": uuid.uuid4().hex,
                "data": {"type": "bash.consumed", "pid": self._pid, "command": command},
            }
        )

    async def _wait_reaped(self) -> None:
        loop = asyncio.get_running_loop()
        future: asyncio.Future[None] = loop.create_future()

        def wake() -> None:
            try:
                loop.call_soon_threadsafe(lambda: future.done() or future.set_result(None))
            except RuntimeError:
                pass

        with self._callback_lock:
            if self._reaped:
                return
            self._reap_callback = wake
        try:
            await future
        finally:
            with self._callback_lock:
                if self._reap_callback is wake:
                    self._reap_callback = None

    async def _wait(self) -> BashResult:
        # Asyncio-native wakeup: no executor thread is parked for the command's
        # duration, so many concurrent awaits cannot exhaust the default pool.
        loop = asyncio.get_running_loop()
        fut: asyncio.Future[None] = loop.create_future()

        def _wake() -> None:
            try:
                loop.call_soon_threadsafe(lambda: fut.done() or fut.set_result(None))
            except RuntimeError:
                pass  # awaiting loop already closed

        self._add_done_callback(_wake)
        await fut
        assert self._result is not None
        return self._result

    async def _wait_owned(self) -> BashResult:
        # One-shot `await bash(cmd)` owns the process: a cancelled await (e.g.
        # a kernel interrupt) must not leave the command running. TERM, bounded
        # grace, group KILL, then a bounded confirmed-exit wait before the
        # CancelledError propagates, so no side effect can land after it.
        try:
            return await self._wait()
        except asyncio.CancelledError:
            # Signal synchronously first: even if the cleanup awaits below are
            # re-cancelled, TERM is already delivered and the escalation armed.
            # The confirm wait runs as a shielded task so repeated cancels of
            # this task cannot skip it; the loop re-awaits until it finishes
            # (the confirm itself is bounded).
            self.kill(grace=_CANCEL_TERM_GRACE)
            confirm = asyncio.ensure_future(self._confirm_group_exit())
            while not confirm.done():
                try:
                    await asyncio.shield(confirm)
                except asyncio.CancelledError:
                    continue
            raise

    async def _confirm_group_exit(self) -> None:
        try:
            await _arequest(
                {
                    "type": "bash.confirmExit",
                    "id": self._activity_id,
                    "termGraceMs": int(_CANCEL_TERM_GRACE * 1000),
                    "killWaitMs": int(_CANCEL_KILL_WAIT * 1000),
                }
            )
        except (BashHostUnavailable, OSError, RuntimeError):
            pass  # the host is gone, and the command with it

    def _group_alive(self) -> bool:
        """Whether any process of the command's group is still alive (the
        host's view: process-group membership, or job accounting on Windows)."""
        if self._reaped:
            return False
        try:
            return bool(_raise_for(_request({"type": "bash.groupAlive", "id": self._activity_id})).get("alive"))
        except KeyError:
            return False

    async def _await_group_death(self, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        while self._group_alive():
            if time.monotonic() >= deadline:
                return False
            await asyncio.sleep(0.02)
        return True

    def __await__(self) -> Generator[Any, None, BashResult]:
        # A handle awaited before any other API use is a one-shot command tied
        # to the await (kill-on-cancel); touching the handle API first marks it
        # as a deliberate background handle whose awaits only wait.
        try:
            current_task = asyncio.current_task()
        except RuntimeError:
            current_task = None
        creating_cell_waited = _creating_cell_waits_for(self._creating_cell_task, current_task)
        owned = not self._released
        wait = self._wait_owned() if owned else self._wait()
        self._released = True
        completed = False
        try:
            result = yield from wait.__await__()
            completed = True
            return result
        finally:
            if (completed or owned) and (
                creating_cell_waited or _creating_cell_waits_for(self._creating_cell_task, current_task)
            ):
                self._awaited_by_creating_cell = True
            if completed:
                self._note_result_consumed(current_task)

    def __reduce__(self) -> NoReturn:
        # Reviving a handle would resurrect a stale pid and a job the new
        # kernel does not own, so a snapshot must never persist a live handle.
        raise TypeError("cannot pickle 'BashHandle' object: live process handle")

    def __repr__(self) -> str:
        state = f"exit_code={self._result.exit_code}" if self._result else "running"
        return f"<BashHandle pid={self._pid} {state} command={self.command!r}>"


def _run_kernel_bash_guards(
    command: str,
    script: str,
    command_prefix: str | None,
    *,
    allow_destructive_git: bool = False,
    allow_destructive_chmod: bool = False,
    allow_force_push: bool = False,
    allow_secret_echo: bool = False,
    allow_pipe_to_shell: bool = False,
    allow_sudo: bool = False,
) -> None:
    """Run every kernel-bash refusal guard on the text the shell will run.

    One entry point so `bash()` and a directly built `BashHandle` cannot
    disagree about which guards apply. `command` is the caller's text; `script`
    is the text the shell runs; `command_prefix` is the
    `PRIME_AGENT_BASH_COMMAND_PREFIX` value pinned by the caller, or None when
    the script has no trusted prefix region (a caller-supplied script).
    """
    allowed = {
        "destructive_git": allow_destructive_git,
        "destructive_chmod": allow_destructive_chmod,
        "force_push": allow_force_push,
        "secret_echo": allow_secret_echo,
        "pipe_to_shell": allow_pipe_to_shell,
        "sudo": allow_sudo,
    }
    _raise_for(
        _request(
            _kernel_request(
                "bash.check",
                command,
                script,
                command_prefix,
                allow=[key for key, allow in allowed.items() if allow],
                **({"traceparent": trace.format_traceparent(ctx)} if (ctx := trace.current()) else {}),
            )
        )
    )


def bash(
    command: str,
    *,
    allow_destructive_git: bool = False,
    allow_destructive_chmod: bool = False,
    allow_force_push: bool = False,
    allow_secret_echo: bool = False,
    allow_pipe_to_shell: bool = False,
    allow_sudo: bool = False,
) -> BashHandle:
    """Start a shell command immediately; await the handle for the result.

    `await bash(cmd)` is a one-shot: cancelling the await (e.g. an interrupt)
    kills the command's process group. `h = bash(cmd)` used as a background
    handle (any .pid/.running/.output()/.tail()/.poll()/.kill() access before
    the first await) survives cancellation; awaiting it only waits. Leak
    containment is per-platform: process groups plus the orphan journal on
    POSIX; a kill-on-close job object on Windows entered while the child is
    still suspended, so no descendant can escape it and kill()/crash cleanup
    are unconditional -- bash() raises if containment cannot be established.
    Output written after the completion fence (e.g. by an EXIT trap or a
    background job) is not in BashResult.output but stays visible via
    handle.output()/tail().

    Destructive git discard commands (`git checkout -- .`, `git restore .`,
    `git reset --hard`, `git clean` that is not a dry run) are refused while
    the repository they target has uncommitted changes; retry with
    allow_destructive_git=True
    only when the discard is intentional. PI_BASH_ALLOW_DESTRUCTIVE_GIT=1 in
    the launching environment disables the guard for the whole kernel; it is
    read once at kernel start, so writing it mid-session has no effect.

    Recursive-force rm commands (`rm -rf`, `-fr`, `-Rf`,
    `--recursive --force`) are refused when an operand resolves outside the
    current workspace (HOME itself, /, parent directories, other trees,
    relocations through cd/pushd), names a protected dot path (`..`, `.git`,
    `.env`-class), or cannot be checked statically (globs, substitutions,
    stdin lists, eval and `sh -c`/`bash -c` payloads, brace expansion,
    symlinked operands, paths that do not exist yet); retry with
    allow_destructive_rm=True (or
    PI_BASH_ALLOW_DESTRUCTIVE_RM=1 frozen at kernel start) only when the
    deletion is intentional.

    Recursive chmod/chown commands (`chmod -R ...`, `chown -R ...`) are
    refused while any operand they name resolves outside the kernel
    workspace or onto the home directory, a dot-directory (e.g. .git), a
    dotfile, or the filesystem root. Operands are resolved the shell's way:
    variables, globs, `cd`/`pushd` (CDPATH included), `eval` and `sh -c`
    payloads, `env -S` strings, aliases, functions and `hash -p` entries.
    Retry with allow_destructive_chmod=True
    (or start the kernel with PI_BASH_ALLOW_DESTRUCTIVE_CHMOD=1) only when
    the recursion is intentional.

    Force-push commands (`git push --force`, `git push -f`, `+`-prefixed
    refspecs, a mirror or push-refspec configuration the command writes) are
    refused while their target is protected: a refspec naming main/master or
    `@{u}`, every branch under `--all`/`--mirror`, or, when the refspec is
    implicit, the current upstream (probed with `git rev-parse @{u}`). A git
    alias is followed, whether the command line defines it or the
    configuration does (read with `git config --get alias.NAME`). The probes
    are bounded and synchronous, and they cannot run anything a command or a
    configuration names (fsmonitor, hooks, pagers, filter and diff drivers,
    ssh commands are all disarmed). A literal `--force-with-lease` or
    `--force-if-includes` is never refused. Retry a deliberate force-push with
    bash(command, allow_force_push=True), or start the kernel with
    PI_BASH_ALLOW_FORCE_PUSH=1.

    Commands that echo secrets into the transcript are refused before any
    process starts, because that output persists in session logs that models
    and users read later: an environment dump (`env`, `printenv`, `export
    -p`, `set`, `$(env)` run as a command) or a read (`cat`, `head`, `tail`,
    ...) of a known secret file (a private SSH key, AWS credentials, a GnuPG
    private key, `.netrc` and similar token files, a process `environ`)
    whose output reaches the transcript. Output sent to a file or a
    variable, or through a filter that keeps one variable (`env | grep
    SAFE_VAR`, `env | cut -d= -f1`), is allowed; grep context lines and a
    zero `--max-count` are not a filter. Read one value instead (`printenv
    SAFE_VAR`), and retry with allow_secret_echo=True (or start the kernel
    with PI_BASH_ALLOW_SECRET_ECHO=1) only when the full output is
    intentional; the env var is read once at kernel start, so writing it
    mid-session never unlocks the guard.

    Downloads that a shell interpreter would run are refused before any
    process starts: a `curl`/`wget` whose output reaches a shell's code,
    through a pipe (`curl -fsSL URL | sh`, `| sudo bash`, `| xargs sh`), a
    substitution or process substitution (`sh -c "$(curl ...)"`, `bash <(curl
    ...)`), or `eval`, at any nesting the guard can read. Download the script
    to a file, read the file, then run it in a later command, and retry with
    allow_pipe_to_shell=True (or start the kernel with
    PI_BASH_ALLOW_PIPE_TO_SHELL=1) only when the download is trusted; the env
    var is frozen at kernel start, so writing it mid-session never unlocks
    the guard.

    Every guard refuses on evidence. The command is parsed once into a model
    of what runs, where, with what input and output; scripts the command runs
    are read wherever they live (`bash x.sh`, `source x.sh`, `./x.sh`, a
    login shell's profile, `$BASH_ENV`). Code the guard cannot read (an
    unreadable script, a shell reading a pipe it cannot reconstruct, a
    command word decided at run time, nesting past the parser's bound) is
    refused only when its visible text carries the guard's own evidence
    (`push` and a force flag, `chmod -R`, `sudo`, `curl`, `env`, a discard
    verb), and the message names that evidence.

    A command that invokes sudo or doas is refused before any process starts,
    because root escapes the containment every other guard relies on. Bypass it
    deliberately with allow_sudo=True, or by starting the kernel with
    PI_BASH_ALLOW_SUDO=1 (honored only when set at kernel start, so a mid-session
    environment write cannot disable the guard).
    """
    if not isinstance(command, str) or not command:
        raise TypeError("command must be a non-empty str")
    _install_shutdown_hook()
    # One read of the prefix feeds the guards and the spawn, so a mid-call
    # environment change cannot make the executed script differ from the text
    # the guards scanned. `_with_prefix` is called once for the script itself.
    command_prefix = os.environ.get("PRIME_AGENT_BASH_COMMAND_PREFIX")
    script = _with_prefix(command, command_prefix)
    allowed = {
        "destructive_git": allow_destructive_git,
        "destructive_chmod": allow_destructive_chmod,
        "force_push": allow_force_push,
        "secret_echo": allow_secret_echo,
        "pipe_to_shell": allow_pipe_to_shell,
        "sudo": allow_sudo,
    }
    # The guards and the spawn are one host request on that one script; a
    # refusal raises here, before any handle (or process) exists.
    launch = _launch(command, script, command_prefix, [key for key, allow in allowed.items() if allow])
    _handoff.launch = launch
    try:
        handle = BashHandle(command, script=script, _validated=True)
    finally:
        _handoff.launch = None
    from . import repl

    repl.emit(
        {
            _BASH_COMMAND_MIME: {
                "command": _capped(command),
                "lines": sum(1 for line in command.splitlines() if line.strip()),
            }
        }
    )
    if launch.interrupt is not None:
        # Interrupted while the run waited: the command is already killed and
        # settled (as a cancelled one-shot await), and the interrupt goes on.
        raise launch.interrupt
    return handle


def _shell() -> str:
    """The shell a command runs in right now (read per call, so env changes
    made in the REPL apply to later commands)."""
    return str(_raise_for(_request(_kernel_request("bash.shell", "", "", None)))["shell"])


def _prefix_command(command: str, prefix: str | None) -> str:
    """The shell script for `command`, `prefix` prepended as its own line.

    The prefix text is threaded alongside the script so the guards scan the
    exact text the handle runs; the format lives in one place.
    """
    return f"{prefix}\n{command}" if prefix else command


_PREFIX_UNSET: Any = object()


def _with_prefix(command: str, prefix: Any = _PREFIX_UNSET) -> str:
    """The command as the kernel runs it: the setup prefix on its own line,
    when one is set. `prefix` pins the value so one caller can share a single
    environment read between the guards and the spawn; pass None explicitly to
    pin "no prefix" without re-reading the environment."""
    if prefix is _PREFIX_UNSET:
        prefix = os.environ.get("PRIME_AGENT_BASH_COMMAND_PREFIX")
    return _prefix_command(command, prefix)


def _child_env(ctx: trace.TraceContext | None = None) -> dict[str, str]:
    """The environment a command spawned now would get: the kernel environment
    with the non-interactive settings (no editor, pager or credential prompt can
    hang a stdin-less agent shell), the guard bypass variables the kernel was
    not launched with dropped, `BASH_ENV`/`ENV`/`BASH_FUNC_*` dropped, and
    TRACEPARENT carrying ``ctx`` (default: the calling cell's context)."""
    ctx = ctx if ctx is not None else trace.current()
    extra = {"traceparent": trace.format_traceparent(ctx)} if ctx is not None else {}
    env = _raise_for(_request(_kernel_request("bash.childEnv", "", "", None, **extra)))["env"]
    return {str(name): str(value) for name, value in env.items()}


_SECRET_PATTERNS = (
    re.compile(r"(?i)(bearer\s+)[^\s'\"]+"),
    re.compile(r"(?i)((?:api[_-]?key|access[_-]?token|refresh[_-]?token|password|secret)\s*[=:]\s*)[^\s'\"]+"),
    re.compile(r"(?i)((?:--?(?:api[_-]?key|token|password|secret))(?:=|\s+))[^\s'\"]+"),
)


def _redact_command(command: str) -> str:
    redacted = command
    for pattern in _SECRET_PATTERNS:
        redacted = pattern.sub(r"\1[REDACTED]", redacted)
    return redacted


def _safe_command(command: str) -> str:
    return _truncate(_redact_command(command))


def active_bash_commands(limit: int = _ACTIVE_INVENTORY_LIMIT) -> list[dict[str, Any]]:
    """Return bounded immutable snapshots of active command progress.

    Records are newly allocated dictionaries and never expose live handles.
    Commands are redacted and truncated before they enter observability state.
    """
    if not isinstance(limit, int):
        raise TypeError("limit must be an int")
    limit = max(0, min(limit, _ACTIVE_INVENTORY_LIMIT))
    with _live_lock:
        handles = {handle._activity_id: handle for handle in _live_handles}
    if not handles or limit == 0:
        return []
    reply = _raise_for(_request({"type": "bash.inventory", "limit": limit}))
    records: list[dict[str, Any]] = []
    for row in reply.get("records", ()):
        handle = handles.get(row.get("id"))
        if handle is None:
            continue
        records.append({"bash.command": handle._span.attrs["bash.command"], **row.get("fields", {})})
    return records


def _truncate(value: str, limit: int = 200) -> str:
    return value if len(value) <= limit else value[: limit - 3] + "..."


def _signal_name(signum: int) -> str:
    try:
        return signal.Signals(signum).name
    except ValueError:
        return str(signum)


def _kill_live_handles() -> None:
    with _live_lock:
        handles = list(_live_handles)
    if not handles:
        return
    for handle in handles:
        # The kernel is going away with the command still running: close its
        # span now (error "kernel shutdown"); the host may already be gone by
        # the time the follower would report the kill.
        try:
            handle._killed = True
            handle._end_span(error="kernel shutdown")
        except BaseException:  # noqa: BLE001 - tracing must never block the kill
            pass
    # SIGKILL every live group. Signal delivery is not proof of group death:
    # the host records a job inactive only once it confirms the reap, so an
    # undelivered kill keeps the active journal row for crash recovery.
    try:
        _request({"type": "bash.killAll"})
    except (BashHostUnavailable, OSError, RuntimeError):
        pass


def _install_shutdown_hook() -> None:
    global _hook_installed
    with _hook_lock:
        if _hook_installed:
            return
        _hook_installed = True
    atexit.register(_kill_live_handles)


def activity_request(action: str, activity_id: str | None = None, lines: int = 50) -> dict[str, Any]:
    """Inspect/stop only commands this kernel started; called off the cell queue."""
    reply = _raise_for(_request({"type": "bash.activity", "action": action, "activityId": activity_id, "lines": lines}))
    reply.pop("status", None)
    return reply
