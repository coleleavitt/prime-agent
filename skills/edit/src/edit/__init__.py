"""Exact single-occurrence string replacement for existing files."""

from __future__ import annotations

from pathlib import Path


async def run(path: str, old_str: str, new_str: str) -> str:
    """Replace a unique string in a file.

    ``old_str`` must appear exactly once in the file at ``path``; that match is
    replaced with ``new_str`` and the file is written back in place. Prefer this
    over rewriting a whole file for targeted edits.

    Args:
        path: File to edit, relative to the working directory, absolute, or
            `~`-prefixed (the leading `~`/`~user` is expanded to the home dir).
        old_str: Exact text to find. Must occur exactly once in the file.
        new_str: Replacement text.

    Returns:
        A short confirmation message.

    Raises:
        FileNotFoundError: If ``path`` does not exist.
        ValueError: If ``old_str`` is absent or matches more than once.
        PermissionError: If the session's permission rules refuse the edit.
    """
    filepath = Path(path).expanduser()
    if not filepath.exists():
        raise FileNotFoundError(f"{path} not found")
    await _check_edit_permission(str(filepath.resolve()))
    content = filepath.read_text(encoding="utf-8")
    count = content.count(old_str)
    if count == 0:
        raise ValueError(f"string not found in {path}")
    if count > 1:
        raise ValueError(
            f"found {count} occurrences in {path}, need exactly 1 — "
            "widen the snippet to make it unique"
        )
    match_index = content.index(old_str)
    start_line = content.count("\n", 0, match_index) + 1
    resolved_path = str(filepath.resolve())
    # The host backs the prior bytes up for /rewind before the write and
    # records the result after it; the write itself stays here.
    await _report_write("file_history.before_write", resolved_path)
    filepath.write_text(content.replace(old_str, new_str, 1), encoding="utf-8")
    await _report_write("file_history.after_write", resolved_path)
    _emit_diff(resolved_path, old_str, new_str, start_line)
    return f"Edited {resolved_path}"


# The host request a file write asks permission with (pa-permissions'
# `file.edit`: Claude Code's `Edit` rules and modes). A host without a
# handler for it answers `UNSERVED_HOST_REQUEST`, and the edit goes ahead.
_EDIT_REQUEST = "file.edit"


async def _check_edit_permission(path: str) -> None:
    """Ask the host whether ``path`` may be edited; raise when refused.

    Outside the kernel, or on a host that does not gate edits, the edit goes
    ahead.
    """
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
    """Report one side of a write to the host's file history (best effort).

    Outside the kernel, in a session without checkpoints, or on any host
    error the edit proceeds unrecorded.
    """
    try:
        from rlm import host_request
    except ImportError:
        return
    try:
        await host_request(request_type, {"path": path})
    except Exception:  # noqa: BLE001 - bookkeeping must never fail the edit
        pass


# Keep in sync with DIFF_DISPLAY_MIME in src/core/kernel/index.ts.
_DIFF_DISPLAY_MIME = "application/vnd.prime-agent.diff+json"


def _emit_diff(path: str, old_str: str, new_str: str, start_line: int) -> None:
    """Stream a diff to the host as a display event; best-effort outside the kernel."""
    try:
        from rlm import emit

        emit(
            {
                _DIFF_DISPLAY_MIME: {
                    "path": path,
                    "old_str": old_str,
                    "new_str": new_str,
                    "start_line": start_line,
                },
                "text/plain": f"Edited {path}",
            }
        )
    except Exception:
        pass
