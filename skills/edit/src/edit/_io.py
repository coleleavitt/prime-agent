"""The write path every edit-skill function shares.

One file write goes: the read-state check (Claude Code's read-before-edit and
modified-since-read rules, `rlm.file_access`), the host's permission check
(`file.edit`: Claude Code's `Edit` rules and modes), the /rewind checkpoint
reports around the write (`file_history.before_write` / `after_write`), the
write itself, and the diff display. Outside the kernel each host step is
skipped and the write goes ahead.
"""

from __future__ import annotations

import os
from pathlib import Path

# The host request a file write asks permission with (pa-permissions'
# `file.edit`). A host without a handler for it answers
# `UNSERVED_HOST_REQUEST`, and the write goes ahead.
_EDIT_REQUEST = "file.edit"

# Keep in sync with DIFF_DISPLAY_MIME in crates/pa-core/src/kernel/shared.rs.
_DIFF_DISPLAY_MIME = "application/vnd.prime-agent.diff+json"
# A whole-file diff past this many characters per side is not displayed
# (the write still lands and the text/plain line still shows).
_DIFF_DISPLAY_LIMIT = 256 * 1024


def _target(path: str) -> tuple[Path, str]:
    """The file ``path`` names (``~`` expanded) and its absolute spelling."""
    filepath = Path(path).expanduser()
    return filepath, os.path.abspath(filepath)


def _read_refusal(absolute: str) -> str | None:
    """Why the kernel's read state refuses a write to ``absolute``, if it does."""
    try:
        from rlm import file_access
    except ImportError:
        return None
    try:
        return file_access.edit_refusal(absolute)
    except Exception:  # noqa: BLE001 - a broken read state must never block a write
        return None


def _note_written(absolute: str) -> None:
    try:
        from rlm import file_access

        file_access.note_written(absolute)
    except Exception:  # noqa: BLE001 - bookkeeping
        pass


async def _check_permission(path: str) -> None:
    """Ask the host whether ``path`` may be written; raise when refused."""
    try:
        from rlm import host_request
        from rlm.repl import UNSERVED_HOST_REQUEST, HostRequestUnavailable
    except ImportError:
        return
    try:
        await host_request(_EDIT_REQUEST, {"path": path})
    except HostRequestUnavailable:
        return
    except RuntimeError as error:
        if UNSERVED_HOST_REQUEST in str(error):
            return
        raise PermissionError(str(error)) from error


async def _report_write(request_type: str, path: str) -> None:
    """Report one side of a write to the host's file history (best effort)."""
    try:
        from rlm import host_request
    except ImportError:
        return
    try:
        await host_request(request_type, {"path": path})
    except Exception:  # noqa: BLE001 - bookkeeping must never fail the write
        pass


async def _write_bytes(filepath: Path, resolved: str, data: bytes, *, create_parents: bool = False) -> None:
    """Write ``data`` to ``filepath`` in place, bracketed by the /rewind reports.

    In place (not a rename) so the file keeps its inode, mode and links.
    """
    await _report_write("file_history.before_write", resolved)
    if create_parents:
        filepath.parent.mkdir(parents=True, exist_ok=True)
    with open(filepath, "wb") as handle:
        handle.write(data)
    await _report_write("file_history.after_write", resolved)


def _emit_diff(path: str, old_str: str, new_str: str, start_line: int, summary: str) -> None:
    """Stream a diff to the host as a display event; best effort outside the kernel."""
    try:
        from rlm import emit
    except ImportError:
        return
    data: dict = {"text/plain": summary}
    if len(old_str) <= _DIFF_DISPLAY_LIMIT and len(new_str) <= _DIFF_DISPLAY_LIMIT:
        data[_DIFF_DISPLAY_MIME] = {
            "path": path,
            "old_str": old_str,
            "new_str": new_str,
            "start_line": start_line,
        }
    try:
        emit(data)
    except Exception:  # noqa: BLE001 - display only
        pass
