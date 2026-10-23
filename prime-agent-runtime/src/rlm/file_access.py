"""Which files a cell opened, for the host's changed-file and re-read reminders.

The model reads files with plain Python (``open``, ``Path.read_text``), so there
is no read tool to observe. An audit hook sees every ``open`` audit event
instead; while a cell's recorder is current (the REPL sets it for the cell's
body and every task the body spawns), each regular file the cell opened is
kept as a read or a write. The REPL ships the lists in the cell's ``done``
frame (``fileAccess``); the host stats and hashes them and compares later.

Interpreter, site-packages, bytecode and the session's own bookkeeping files
are never recorded: they are not what the model read. The recorder is bounded
per cell; this is a context aid, not an audit log.
"""

from __future__ import annotations

import contextvars
import os
import stat
import sys
import threading
from typing import Any

# Per-cell bound on each list; the first paths win (later ones are dropped).
CELL_PATH_LIMIT = 64

_WRITE_MODE_CHARS = frozenset("wax+")
_WRITE_OPEN_FLAGS = os.O_WRONLY | os.O_RDWR | os.O_APPEND | os.O_CREAT | os.O_TRUNC
_SKIPPED_SUFFIXES = (".pyc", ".pyo", ".so", ".pyd", ".dylib")
_SKIPPED_PARTS = (
    f"{os.sep}__pycache__{os.sep}",
    f"{os.sep}site-packages{os.sep}",
    f"{os.sep}dist-packages{os.sep}",
)
_SKIPPED_ROOTS_POSIX = ("/proc", "/sys", "/dev")


class CellFileAccess:
    """The files one cell opened, split into reads and writes."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._reads: list[str] = []
        self._writes: list[str] = []
        self._closed = False

    def record(self, path: str, write: bool) -> None:
        with self._lock:
            if self._closed:
                return
            target = self._writes if write else self._reads
            if path in target or len(target) >= CELL_PATH_LIMIT:
                return
            target.append(path)

    def close(self) -> dict[str, list[str]] | None:
        """Stop recording; the frame field, or None when the cell opened nothing."""
        with self._lock:
            self._closed = True
            if not self._reads and not self._writes:
                return None
            field: dict[str, list[str]] = {}
            if self._reads:
                field["reads"] = list(self._reads)
            if self._writes:
                field["writes"] = list(self._writes)
            return field


_current: contextvars.ContextVar[CellFileAccess | None] = contextvars.ContextVar(
    "_rlm_cell_file_access", default=None
)
_installed = False
_skipped_roots: tuple[str, ...] = ()


def _root_prefixes() -> tuple[str, ...]:
    roots: set[str] = set()
    for root in (sys.prefix, sys.base_prefix, sys.exec_prefix, sys.base_exec_prefix):
        if root:
            roots.add(os.path.realpath(root))
    for name in ("RLM_SESSION_DIR", "PRIME_AGENT_CODING_AGENT_DIR"):
        value = os.environ.get(name)
        if value:
            roots.add(os.path.realpath(value))
    # The runtime's own package (a checkout's `src/` when not installed).
    roots.add(os.path.dirname(os.path.realpath(__file__)))
    if os.name == "posix":
        roots.update(_SKIPPED_ROOTS_POSIX)
    return tuple(sorted(root.rstrip(os.sep) + os.sep for root in roots))


def is_write(mode: Any, flags: Any) -> bool:
    """Whether an ``open`` audit event's mode/flags ask for write access."""
    if isinstance(mode, str):
        return any(char in _WRITE_MODE_CHARS for char in mode)
    return isinstance(flags, int) and bool(flags & _WRITE_OPEN_FLAGS)


def recorded_path(path: Any) -> str | None:
    """The absolute path an ``open`` of ``path`` records, or None when it is skipped."""
    if isinstance(path, int):
        return None
    try:
        text = os.path.abspath(os.fsdecode(path))
    except (TypeError, ValueError, OSError):
        return None
    if text.endswith(_SKIPPED_SUFFIXES) or any(part in text for part in _SKIPPED_PARTS):
        return None
    if any(text.startswith(root) for root in _skipped_roots):
        return None
    return text


def _hook(event: str, args: tuple[Any, ...]) -> None:
    if event != "open":
        return
    recorder = _current.get()
    if recorder is None or len(args) < 3:
        return
    path, mode, flags = args[0], args[1], args[2]
    recorded = recorded_path(path)
    if recorded is None:
        return
    write = is_write(mode, flags)
    if not write:
        # Directories and devices are not file reads; a write may create the file.
        try:
            if not stat.S_ISREG(os.stat(recorded).st_mode):
                return
        except OSError:
            return
    recorder.record(recorded, write)


def install() -> None:
    """Install the audit hook once (a hook cannot be removed)."""
    global _installed, _skipped_roots
    if _installed:
        return
    _skipped_roots = _root_prefixes()
    sys.addaudithook(_hook)
    _installed = True


def begin_cell() -> tuple[CellFileAccess, contextvars.Token[CellFileAccess | None]]:
    """Start recording for the cell running in the current context."""
    recorder = CellFileAccess()
    return recorder, _current.set(recorder)


def reset_cell(token: contextvars.Token[CellFileAccess | None]) -> None:
    """Restore the context's recorder from before :func:`begin_cell`."""
    _current.reset(token)
