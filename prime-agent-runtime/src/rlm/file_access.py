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

The same hook keeps the kernel's read state (Claude Code's ``readFileState``):
for each file the model last read, the file's mtime and size at that read, and
for each file the kernel itself wrote, that the model knows its content. The
``edit`` skill asks :func:`edit_refusal` before it writes, so an edit to a file
that changed on disk since the model read it is refused (Claude Code's "File
has been modified since read" check) and, under ``kernel.editReadCheck:
"read"``, so is an edit to a file the model never read.
"""

from __future__ import annotations

import contextvars
import os
from collections import OrderedDict
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

# The read state's bound: the least recently opened paths are forgotten first.
READ_STATE_LIMIT = 4096
# The host's `kernel.editReadCheck` setting reaches the kernel here:
# `off`, `stale` (the default: refuse an edit to a file that changed since the
# model read it) or `read` (also refuse an edit to an existing file the model
# never read, Claude Code's Edit rule).
EDIT_READ_CHECK_ENV = "PRIME_AGENT_EDIT_READ_CHECK"
NOT_READ_MESSAGE = "File has not been read yet. Read it first before writing to it."
MODIFIED_SINCE_READ_MESSAGE = (
    "File has been modified since read, either by the user or by a linter. "
    "Read it again before attempting to write it."
)
# A path whose last access was a kernel write: the model knows its content.
_WRITTEN = (-1, -1)


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
_read_state: OrderedDict[str, tuple[int, int]] = OrderedDict()
_read_state_lock = threading.Lock()


def _remember(path: str, stamp: tuple[int, int]) -> None:
    with _read_state_lock:
        _read_state[path] = stamp
        _read_state.move_to_end(path)
        while len(_read_state) > READ_STATE_LIMIT:
            _read_state.popitem(last=False)


def _stamp(st: os.stat_result) -> tuple[int, int]:
    return (st.st_mtime_ns, st.st_size)


def note_written(path: str) -> None:
    """Record that the kernel just wrote ``path`` with content the model chose.

    The write helpers call this after their write, so a later change by
    someone else is caught; a plain Python write only marks the path known.
    """
    absolute = os.path.abspath(path)
    try:
        stamp = _stamp(os.stat(absolute))
    except OSError:
        stamp = _WRITTEN
    for key in {absolute, os.path.realpath(absolute)}:
        _remember(key, stamp)


def edit_check_mode() -> str:
    """The `kernel.editReadCheck` mode: `off`, `stale` (default) or `read`."""
    value = (os.environ.get(EDIT_READ_CHECK_ENV) or "").strip().lower()
    return value if value in ("off", "stale", "read") else "stale"


def edit_refusal(path: str) -> str | None:
    """Why the kernel must not write ``path`` now, or None when it may.

    Only an existing file can be refused: creating a file needs no read.
    """
    mode = edit_check_mode()
    if mode == "off":
        return None
    absolute = os.path.abspath(path)
    try:
        current = _stamp(os.stat(absolute))
    except OSError:
        return None
    # The model may have read the file through a symlink, or the writer may
    # name its target: either spelling counts.
    with _read_state_lock:
        known = _read_state.get(absolute)
        if known is None:
            known = _read_state.get(os.path.realpath(absolute))
    if known is None:
        return NOT_READ_MESSAGE if mode == "read" else None
    if known != _WRITTEN and known != current:
        return MODIFIED_SINCE_READ_MESSAGE
    return None


def forget_read_state() -> None:
    """Drop the whole read state (tests)."""
    with _read_state_lock:
        _read_state.clear()


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
    if write:
        _remember(recorded, _WRITTEN)
    else:
        # Directories and devices are not file reads; a write may create the file.
        try:
            st = os.stat(recorded)
        except OSError:
            return
        if not stat.S_ISREG(st.st_mode):
            return
        _remember(recorded, _stamp(st))
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
