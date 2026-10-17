"""Worktree isolation mid-session (Claude Code's EnterWorktree / ExitWorktree).

``rlm.enter_worktree(name)`` asks the host to create (or reuse) a managed git
worktree under ``<repo>/.prime/worktrees/<name>`` on the branch
``worktree-<name>`` and moves the session into it; ``rlm.exit_worktree(action)``
moves it back, keeping the worktree or removing it. The host records the move
(a resume restarts in the worktree) and the kernel changes directory here, so
``bash()`` and relative paths follow at once.

Use these only when the user (or the project instructions) explicitly ask to
work in a worktree; a branch switch is a plain git command.
"""

from __future__ import annotations

import os
from typing import Any


async def enter_worktree(name: str | None = None, *, path: str | None = None) -> dict[str, Any]:
    """Create (or reuse) a managed worktree and work in it.

    ``name``: the worktree's name ("/"-separated segments of letters, digits,
    ".", "_" and "-", at most 64 characters); a name is generated when omitted.
    ``path``: switch into an existing linked worktree of this repository
    instead (it is never removed on exit). Returns the host's answer:
    ``worktreePath``, ``worktreeBranch``, ``existed`` and a ``message``.
    """
    if name is not None and path is not None:
        raise ValueError("Provide at most one of `name` or `path`, not both.")
    payload: dict[str, Any] = {}
    if name is not None:
        payload["name"] = str(name)
    if path is not None:
        payload["path"] = os.path.abspath(os.path.expanduser(str(path)))
    from . import host_request

    result = await host_request("worktree.enter", payload)
    os.chdir(result["worktreePath"])
    return result


async def exit_worktree(action: str, *, discard_changes: bool = False) -> dict[str, Any]:
    """Leave the worktree session ``enter_worktree`` started.

    ``action``: ``"keep"`` leaves the worktree and its branch on disk;
    ``"remove"`` deletes them. A worktree with uncommitted files or new
    commits is refused for removal (the error lists them) unless
    ``discard_changes=True``; confirm with the user first. A worktree entered
    by ``path`` is never removed. Without an active worktree session this
    changes nothing.
    """
    if action not in ("keep", "remove"):
        raise ValueError('action must be "keep" or "remove"')
    from . import host_request

    result = await host_request("worktree.exit", {"action": action, "discardChanges": bool(discard_changes)})
    restored = result.get("restoredCwd")
    if isinstance(restored, str) and restored:
        os.chdir(restored)
    return result
