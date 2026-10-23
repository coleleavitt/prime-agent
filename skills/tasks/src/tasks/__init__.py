"""Prime Agent tasks skill: a persistent per-session task list.

The model tracks multi-step work here: create tasks, move each one through
``pending -> in_progress -> completed`` as the work happens, record which tasks
block which, and list them. The list is one JSON file in the session's artifact
directory (``$RLM_SESSION_DIR/tasks.json``), rewritten atomically on every
change, so it survives a kernel restart and a session resume. Without a session
directory the list lives in kernel memory only, and says so.

Modelled on Claude Code's TaskCreate / TaskGet / TaskUpdate / TaskList.
"""

from __future__ import annotations

import builtins
import contextlib
import json
import os
import sys
import tempfile
import threading
from collections.abc import Iterable, Mapping
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Literal

__all__ = ["TaskList", "create", "get", "list", "update"]

Status = Literal["pending", "in_progress", "completed", "deleted"]

_STATUSES = ("pending", "in_progress", "completed", "deleted")
_OPEN_STATUSES = ("pending", "in_progress")
_FILE_NAME = "tasks.json"
_FORMAT_VERSION = 1
_MEMORY_NOTE = (
    "no session directory (RLM_SESSION_DIR is unset): this task list lives in kernel "
    "memory only and is lost when the kernel restarts"
)

# One read-modify-write at a time: cells, their asyncio tasks and user threads
# share this module. Every public call does its file work without awaiting, so
# the lock is never held across an event-loop turn.
_LOCK = threading.Lock()
_memory_store: dict[str, Any] | None = None
_memory_note_shown = False

_builtin_list = builtins.list


class TaskList(_builtin_list):
    """``tasks.list()``'s result: summary dicts ordered by id.

    A plain list of dicts (``id``, ``subject``, ``status``, ``owner``,
    ``blocked_by``) whose ``repr`` is a compact checklist, so a cell ending in
    ``await tasks.list()`` shows the checklist in the transcript.
    """

    text: str

    def __init__(self, rows: Iterable[dict[str, Any]] = (), text: str = "") -> None:
        super().__init__(rows)
        self.text = text

    def __repr__(self) -> str:
        return self.text

    def __str__(self) -> str:
        return self.text


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _empty_store() -> dict[str, Any]:
    return {"version": _FORMAT_VERSION, "next_id": 1, "tasks": []}


def _store_path() -> Path | None:
    raw = (os.environ.get("RLM_SESSION_DIR") or "").strip()
    if not raw:
        return None
    return Path(raw).expanduser() / _FILE_NAME


def _load(path: Path | None) -> dict[str, Any]:
    global _memory_store, _memory_note_shown
    if path is None:
        if not _memory_note_shown:
            _memory_note_shown = True
            print(f"tasks: {_MEMORY_NOTE}", file=sys.stderr)
        if _memory_store is None:
            _memory_store = _empty_store()
        return json.loads(json.dumps(_memory_store))
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return _empty_store()
    try:
        store = json.loads(text)
    except ValueError as error:
        raise RuntimeError(f"task list {path} is not valid JSON ({error}); fix or remove it") from None
    if (
        not isinstance(store, dict)
        or not isinstance(store.get("next_id"), int)
        or not isinstance(store.get("tasks"), _builtin_list)
        or not all(isinstance(task, dict) and isinstance(task.get("id"), str) for task in store["tasks"])
    ):
        raise RuntimeError(f"task list {path} has an unexpected shape; fix or remove it")
    return store


def _save(path: Path | None, store: dict[str, Any]) -> None:
    global _memory_store
    # Host-readable write marks: a reminder producer outside the kernel (Claude Code's
    # todo nag) compares ``last_write_at`` with its turn timestamps to count the turns
    # since the list last changed; ``writes`` counts every change.
    store["last_write_at"] = _now()
    store["writes"] = int(store.get("writes") or 0) + 1
    if path is None:
        _memory_store = store
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temp_name = tempfile.mkstemp(prefix=".tasks-", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(store, handle, indent=2, ensure_ascii=False)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp_name, path)
    except BaseException:
        with contextlib.suppress(OSError):
            os.unlink(temp_name)
        raise


def _normalize_id(value: object, name: str = "id") -> str:
    if isinstance(value, bool) or not isinstance(value, (str, int)):
        raise TypeError(f"{name} must be a task id (str or int), got {type(value).__name__}")
    text = str(value).strip().removeprefix("#")
    if not text.isdigit():
        raise ValueError(f"{name} {value!r} is not a task id (ids are '1', '2', ...)")
    return str(int(text))


def _normalize_ids(value: object, name: str) -> _builtin_list[str]:
    if isinstance(value, (str, int)) and not isinstance(value, bool):
        return [_normalize_id(value, name)]
    if isinstance(value, Iterable) and not isinstance(value, (bytes, Mapping)):
        return [_normalize_id(item, name) for item in value]
    raise TypeError(f"{name} must be a task id or a list of task ids, got {type(value).__name__}")


def _id_key(task_id: str) -> int:
    return int(task_id)


def _find(store: dict[str, Any], task_id: str) -> dict[str, Any]:
    for task in store["tasks"]:
        if task["id"] == task_id:
            return task
    raise ValueError(f"no task #{task_id}")


def _require_text(value: object, name: str, *, allow_empty: bool) -> str:
    if not isinstance(value, str):
        raise TypeError(f"{name} must be str, got {type(value).__name__}")
    if not allow_empty and not value.strip():
        raise ValueError(f"{name} must not be empty")
    return value


def _require_metadata(value: object) -> dict[str, Any]:
    if not isinstance(value, Mapping):
        raise TypeError(f"metadata must be a dict, got {type(value).__name__}")
    metadata = {}
    for key, item in value.items():
        if not isinstance(key, str):
            raise TypeError(f"metadata keys must be str, got {type(key).__name__}")
        metadata[key] = item
    try:
        json.dumps(metadata)
    except (TypeError, ValueError) as error:
        raise TypeError(f"metadata must be JSON-serializable: {error}") from None
    return metadata


def _public(task: dict[str, Any], warnings: _builtin_list[str] | None = None) -> dict[str, Any]:
    result = json.loads(json.dumps(task))
    if warnings:
        result["warnings"] = warnings
    return result


def _open_blockers(store: dict[str, Any], task: dict[str, Any]) -> _builtin_list[str]:
    by_id = {other["id"]: other for other in store["tasks"]}
    return [
        blocker
        for blocker in task.get("blocked_by", [])
        if by_id.get(blocker, {}).get("status") in _OPEN_STATUSES
    ]


def _reaches(store: dict[str, Any], start: str, target: str) -> bool:
    """Whether ``target`` is reachable from ``start`` along ``blocks`` edges."""
    by_id = {task["id"]: task for task in store["tasks"]}
    seen: set[str] = set()
    frontier = [start]
    while frontier:
        current = frontier.pop()
        if current == target:
            return True
        if current in seen:
            continue
        seen.add(current)
        frontier.extend(by_id.get(current, {}).get("blocks", []))
    return False


def _link(store: dict[str, Any], blocker: dict[str, Any], blocked: dict[str, Any]) -> None:
    """Record ``blocker`` blocks ``blocked`` on both tasks."""
    if blocker["id"] == blocked["id"]:
        raise ValueError(f"task #{blocker['id']} cannot block itself")
    for task in (blocker, blocked):
        if task["status"] == "deleted":
            raise ValueError(f"task #{task['id']} is deleted")
    if blocked["id"] in blocker["blocks"]:
        return
    if _reaches(store, blocked["id"], blocker["id"]):
        raise ValueError(f"#{blocker['id']} blocking #{blocked['id']} would make a dependency cycle")
    blocker["blocks"] = sorted({*blocker["blocks"], blocked["id"]}, key=_id_key)
    blocked["blocked_by"] = sorted({*blocked["blocked_by"], blocker["id"]}, key=_id_key)


def _unlink_everywhere(store: dict[str, Any], task_id: str) -> None:
    for task in store["tasks"]:
        if task["id"] == task_id:
            continue
        task["blocks"] = [other for other in task.get("blocks", []) if other != task_id]
        task["blocked_by"] = [other for other in task.get("blocked_by", []) if other != task_id]


def _render(rows: _builtin_list[dict[str, Any]], store: dict[str, Any], in_memory: bool) -> str:
    done = sum(1 for row in rows if row["status"] == "completed")
    header = f"Tasks: {done}/{len(rows)} completed" if rows else "Tasks: none"
    if in_memory:
        header += " (in memory only: no session directory)"
    lines = [header]
    by_id = {task["id"]: task for task in store["tasks"]}
    for row in rows:
        task = by_id[row["id"]]
        marker = {"completed": "[x]", "in_progress": "[>]"}.get(row["status"], "[ ]")
        label = row["subject"]
        if row["status"] == "in_progress" and task.get("active_form"):
            label = task["active_form"]
        details = []
        if row["blocked_by"]:
            details.append("blocked by " + ", ".join(f"#{blocker}" for blocker in row["blocked_by"]))
        if row["owner"]:
            details.append(f"owner: {row['owner']}")
        suffix = f" ({'; '.join(details)})" if details else ""
        lines.append(f"  {marker} #{row['id']} {label}{suffix}")
    return "\n".join(lines)


def _warn(warnings: _builtin_list[str]) -> None:
    for warning in warnings:
        print(f"tasks: warning: {warning}", file=sys.stderr)


async def create(
    subject: str,
    description: str = "",
    active_form: str | None = None,
    metadata: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Add a ``pending`` task and return it.

    ``subject`` is a short imperative title ("Run the migrations");
    ``active_form`` is its present-continuous label ("Running the migrations"),
    shown instead of the subject while the task is in progress. Ids are
    ``"1"``, ``"2"``, ... in creation order and are never reused.
    """
    subject = _require_text(subject, "subject", allow_empty=False).strip()
    description = _require_text(description, "description", allow_empty=True)
    if active_form is not None:
        active_form = _require_text(active_form, "active_form", allow_empty=True).strip() or None
    task_metadata = _require_metadata(metadata) if metadata is not None else {}
    with _LOCK:
        path = _store_path()
        store = _load(path)
        task_id = str(store["next_id"])
        now = _now()
        task = {
            "id": task_id,
            "subject": subject,
            "description": description,
            "status": "pending",
            "active_form": active_form,
            "owner": None,
            "blocks": [],
            "blocked_by": [],
            "metadata": {key: value for key, value in task_metadata.items() if value is not None},
            "created_at": now,
            "updated_at": now,
        }
        store["next_id"] += 1
        store["tasks"].append(task)
        _save(path, store)
        return _public(task)


async def get(id: str | int) -> dict[str, Any]:
    """Return one task in full (description, ``blocks``, ``blocked_by``, metadata).

    A deleted task is still returned, with status ``deleted``; an unknown id
    raises ``ValueError``.
    """
    task_id = _normalize_id(id)
    with _LOCK:
        store = _load(_store_path())
        return _public(_find(store, task_id))


async def update(
    id: str | int,
    *,
    status: Status | None = None,
    subject: str | None = None,
    description: str | None = None,
    active_form: str | None = None,
    owner: str | None = None,
    add_blocks: str | int | Iterable[str | int] | None = None,
    add_blocked_by: str | int | Iterable[str | int] | None = None,
    metadata: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Change one task and return it.

    ``status`` is ``pending``, ``in_progress``, ``completed`` or ``deleted``
    (deleted tasks drop out of ``list()`` and out of other tasks' dependency
    lists). ``owner=""`` or ``active_form=""`` clears the field.
    ``add_blocks`` / ``add_blocked_by`` take ids and record the dependency on
    both tasks. ``metadata`` merges into the task's metadata; a ``None`` value
    removes that key. Starting a second ``in_progress`` task, or starting or
    completing a task whose blockers are still open, is allowed but returns
    (and prints) a warning in ``warnings``.
    """
    task_id = _normalize_id(id)
    if status is not None and status not in _STATUSES:
        raise ValueError(f"status must be one of {', '.join(_STATUSES)}; got {status!r}")
    if subject is not None:
        subject = _require_text(subject, "subject", allow_empty=False).strip()
    if description is not None:
        description = _require_text(description, "description", allow_empty=True)
    if active_form is not None:
        active_form = _require_text(active_form, "active_form", allow_empty=True)
    if owner is not None:
        owner = _require_text(owner, "owner", allow_empty=True)
    blocks = _normalize_ids(add_blocks, "add_blocks") if add_blocks is not None else []
    blocked_by = _normalize_ids(add_blocked_by, "add_blocked_by") if add_blocked_by is not None else []
    metadata_patch = _require_metadata(metadata) if metadata is not None else None

    with _LOCK:
        path = _store_path()
        store = _load(path)
        task = _find(store, task_id)
        if task["status"] == "deleted":
            raise ValueError(f"task #{task_id} is deleted")
        warnings: _builtin_list[str] = []

        if subject is not None:
            task["subject"] = subject
        if description is not None:
            task["description"] = description
        if active_form is not None:
            task["active_form"] = active_form.strip() or None
        if owner is not None:
            task["owner"] = owner.strip() or None
        if metadata_patch is not None:
            merged = dict(task.get("metadata") or {})
            for key, value in metadata_patch.items():
                if value is None:
                    merged.pop(key, None)
                else:
                    merged[key] = value
            task["metadata"] = merged
        for other_id in blocks:
            _link(store, task, _find(store, other_id))
        for other_id in blocked_by:
            _link(store, _find(store, other_id), task)

        if status is not None and status != task["status"]:
            if status == "in_progress":
                others = [
                    other["id"]
                    for other in store["tasks"]
                    if other["status"] == "in_progress" and other["id"] != task_id
                ]
                if others:
                    listed = ", ".join(f"#{other}" for other in others)
                    warnings.append(
                        f"{listed} {'is' if len(others) == 1 else 'are'} already in_progress; "
                        "normally only one task is in progress at a time: complete it or set it back to pending"
                    )
            if status in ("in_progress", "completed"):
                open_blockers = _open_blockers(store, task)
                if open_blockers:
                    listed = ", ".join(f"#{blocker}" for blocker in open_blockers)
                    warnings.append(f"#{task_id} is still blocked by open task {listed}")
            task["status"] = status
            if status == "deleted":
                _unlink_everywhere(store, task_id)
                task["blocks"] = []
                task["blocked_by"] = []
        task["updated_at"] = _now()
        _save(path, store)
        _warn(warnings)
        return _public(task, warnings)


async def list() -> TaskList:
    """Return every task that is not deleted, ordered by id, completed ones included.

    Each row is ``{"id", "subject", "status", "owner", "blocked_by"}`` where
    ``blocked_by`` names only blockers that are still open. The result is a
    list whose ``repr`` (and ``.text``) is a compact checklist.
    """
    with _LOCK:
        path = _store_path()
        store = _load(path)
        rows = [
            {
                "id": task["id"],
                "subject": task["subject"],
                "status": task["status"],
                "owner": task.get("owner"),
                "blocked_by": _open_blockers(store, task),
            }
            for task in sorted(store["tasks"], key=lambda task: _id_key(task["id"]))
            if task["status"] != "deleted"
        ]
        return TaskList(rows, _render(rows, store, in_memory=path is None))
