"""Plan-mode (no-edit) fallback guard for the Prime Agent kernel.

Where the OS can confine processes (Landlock on Linux, Seatbelt on macOS) the
host enforces plan mode itself: the kernel restarts under a read-only OS
sandbox, and nothing here is armed. This module is the fallback for a machine
with no OS sandbox: while enabled, filesystem mutations outside the writable
roots and every process spawn raise ``PlanModeError``. Enforcement uses
``sys.addaudithook``: a hook cannot be removed once installed.

Only the host can switch the guard. The REPL claims the one host controller at
startup, before any cell runs, and answers the host's ``plan_guard`` protocol
frame through it; the controller also checks the host-held token the first
frame binds. Kernel code can import this module, read ``is_enabled()`` and call
``check_bash()``, but it cannot claim a second controller or rebind the token.

This is a guard against an agent that edits when it should be planning, not a
sandbox for hostile code: arbitrary Python can still reach the guard's closure
state through interpreter introspection, or call libc through ``ctypes``
(left open: ``dill``, which the namespace snapshot needs, calls into the C API
through it). The host refuses its own mutating tools, and the kernel's
``bash()`` jobs, independently.
"""

from __future__ import annotations

import hashlib
import hmac
import os
import sys
import tempfile
from collections.abc import Callable, Iterable, Sequence
from pathlib import Path
from typing import Any

_PLAN_MODE_MESSAGE = (
    "Plan mode is active: {action} is blocked. You may read files, run "
    "read-only commands, and write only under temp/cache directories. Do not "
    "attempt to work around this. Present your plan or answer, and ask the "
    "user to exit plan mode if edits are needed."
)

_NO_SANDBOX_COMMAND_ACTION = "running commands (this machine has no OS sandbox to run them read-only)"


class PlanModeError(RuntimeError):
    """A mutation refused because plan mode is active."""

    def __init__(self, action: str) -> None:
        super().__init__(_PLAN_MODE_MESSAGE.format(action=action))


# io.open write intent letters / os.open write intent flags.
_WRITE_MODE_CHARS = frozenset("wax+")
_WRITE_OPEN_FLAGS = os.O_WRONLY | os.O_RDWR | os.O_APPEND | os.O_CREAT | os.O_TRUNC

# Audit events that mutate the filesystem; every str/bytes/PathLike argument is
# treated as a target path (rename/link must have BOTH ends writable).
_FS_MUTATION_EVENTS = frozenset(
    {
        "os.remove",
        "os.rename",
        "os.rmdir",
        "os.mkdir",
        "os.chmod",
        "os.chown",
        "os.chflags",
        "os.lchflags",
        "os.link",
        "os.symlink",
        "os.truncate",
        "os.utime",
        "os.setxattr",
        "os.removexattr",
        "shutil.rmtree",
        "shutil.move",
        "shutil.chown",
    }
)

# Copies read their source; only the destination (the second argument) is written.
_FS_COPY_EVENTS = frozenset({"shutil.copyfile", "shutil.copymode", "shutil.copystat"})

# Process spawns: without an OS sandbox nothing can run them read-only.
_SPAWN_EVENTS = frozenset(
    {
        "subprocess.Popen",
        "os.system",
        "os.exec",
        "os.posix_spawn",
        "os.spawn",
        "os.startfile",
        "os.fork",
        "os.forkpty",
        "pty.spawn",
    }
)


def _norm(path: Any) -> str | None:
    try:
        if isinstance(path, int):
            return None
        # realpath: a symlink under a writable root must not reach a file outside it.
        return os.path.realpath(os.fsdecode(path))
    except (TypeError, ValueError):
        return None


def _default_writable_roots() -> set[str]:
    roots = {tempfile.gettempdir(), "/tmp", "/dev", str(Path.home() / ".cache")}
    tmpdir = os.environ.get("TMPDIR")
    if tmpdir:
        roots.add(tmpdir)
    if sys.platform == "darwin":
        roots.update({"/private/tmp", "/private/var/folders"})
    return roots


# (root, writable) pairs, shortest root first: the deepest root containing a
# path decides, and a protected root beats a writable one of the same path.
Roots = tuple[tuple[str, bool], ...]


def _resolve_roots(writable: Iterable[str], protected: Iterable[str] = ()) -> Roots:
    pairs = {(os.path.realpath(r), True) for r in writable if r}
    protected_paths = {os.path.realpath(r) for r in protected if r}
    pairs = {pair for pair in pairs if pair[0] not in protected_paths}
    pairs.update((root, False) for root in protected_paths)
    return tuple(sorted(pairs, key=lambda pair: (len(pair[0]), not pair[1], pair[0])))


def _under(path: str, root: str) -> bool:
    return path == root or path.startswith(root.rstrip(os.sep) + os.sep)


def _is_write_allowed(path: str, roots: Roots) -> bool:
    parts = path.split(os.sep)
    if "__pycache__" in parts or path.endswith(".pyc"):
        return True
    deciding: tuple[str, bool] | None = None
    for root, writable in roots:
        if _under(path, root):
            deciding = (root, writable)
    if deciding is None or not deciding[1]:
        return False
    # Repository metadata stays read-only even inside writable roots.
    rest = path[len(deciding[0]) :].split(os.sep)
    return ".git" not in rest


def _open_wants_write(mode: Any, flags: Any) -> bool:
    if isinstance(mode, str):
        return bool(_WRITE_MODE_CHARS.intersection(mode))
    if isinstance(flags, int):
        return bool(flags & _WRITE_OPEN_FLAGS)
    return False


# (token, enabled, extra_writable_roots, protected_roots) -> enabled
HostController = Callable[[str, bool, Sequence[str], Sequence[str]], bool]


def _make_guard() -> tuple[Callable[[], HostController], Callable[[], bool], Callable[[str], None]]:
    state: dict[str, Any] = {
        "token_hash": None,
        "enabled": False,
        "roots": _resolve_roots(_default_writable_roots()),
        "hook_added": False,
        "claimed": False,
    }

    def _hook(event: str, args: tuple[Any, ...]) -> None:
        if not state["enabled"]:
            return
        if event == "open":
            if len(args) < 3 or not _open_wants_write(args[1], args[2]):
                return
            path = _norm(args[0])
            if path is not None and not _is_write_allowed(path, state["roots"]):
                raise PlanModeError(f"writing to {path}")
        elif event in _FS_MUTATION_EVENTS:
            for arg in args:
                if not isinstance(arg, (str, bytes, os.PathLike)):
                    continue
                path = _norm(arg)
                if path is not None and not _is_write_allowed(path, state["roots"]):
                    raise PlanModeError(f"{event} on {path}")
        elif event in _FS_COPY_EVENTS:
            path = _norm(args[1]) if len(args) > 1 else None
            if path is not None and not _is_write_allowed(path, state["roots"]):
                raise PlanModeError(f"{event} to {path}")
        elif event in _SPAWN_EVENTS:
            raise PlanModeError(_NO_SANDBOX_COMMAND_ACTION)

    def _control(
        token: str,
        enabled: bool,
        extra_writable_roots: Sequence[str],
        protected_roots: Sequence[str] = (),
    ) -> bool:
        if not isinstance(token, str) or not token:
            raise PermissionError("plan_guard: invalid token")
        given = hashlib.sha256(token.encode()).digest()
        if state["token_hash"] is None:
            # The first frame binds the host's token; it arrives before any cell.
            state["token_hash"] = given
        elif not hmac.compare_digest(state["token_hash"], given):
            raise PermissionError("plan_guard: invalid token")
        if enabled:
            roots = _default_writable_roots()
            roots.update(r for r in extra_writable_roots if isinstance(r, str) and r)
            state["roots"] = _resolve_roots(roots, (r for r in protected_roots if isinstance(r, str) and r))
            if not state["hook_added"]:
                sys.addaudithook(_hook)
                state["hook_added"] = True
        state["enabled"] = bool(enabled)
        return bool(state["enabled"])

    def claim_host_controller() -> HostController:
        if state["claimed"]:
            raise PermissionError("plan_guard: the host controller is already claimed")
        state["claimed"] = True
        return _control

    def is_enabled() -> bool:
        return bool(state["enabled"])

    def check_bash(command: str) -> None:
        del command  # every command is refused: nothing can run it read-only
        if state["enabled"]:
            raise PlanModeError(_NO_SANDBOX_COMMAND_ACTION)

    return claim_host_controller, is_enabled, check_bash


claim_host_controller, is_enabled, check_bash = _make_guard()
claim_host_controller.__doc__ = (
    "Return the one host controller ``(token, enabled, extra_writable_roots, protected_roots) -> enabled``; "
    "a second claim raises PermissionError. The REPL claims it at startup."
)
is_enabled.__doc__ = "Whether the in-kernel plan-mode guard is armed in this kernel."
check_bash.__doc__ = (
    "Raise PlanModeError when the in-kernel plan-mode guard is armed: without an OS sandbox "
    "no shell command can be run read-only."
)

__all__ = ["PlanModeError", "check_bash", "claim_host_controller", "is_enabled"]
