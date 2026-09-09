"""Prime Agent ravo skill: full RAVO loop over a harness mutation from the kernel.

The run executes host-side (the same implementation as /ravo); these
functions are thin typed wrappers over the generic host bridge
(`rlm.host_request`). They only work inside the Prime Agent Python kernel.
"""

from __future__ import annotations

from typing import Any

from rlm import host_request


def _check_count(name: str, value: int | None) -> None:
    if value is None:
        return
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        raise TypeError(f"{name} must be a positive int or None, got {value!r}")


async def status() -> dict[str, Any]:
    """Read the current RAVO run status.

    Returns the run status dict (`runId`, `phase`, `round`, `repairs`,
    `stopReason`, `lastCertificate`, ...) or `{"phase": "idle"}` when no run
    is active.
    """
    return await host_request("ravo.status")


async def run(
    task: str,
    instructions: str | None = None,
    global_: bool = False,
    max_rounds: int | None = None,
    max_repairs: int | None = None,
    arc_agi: dict[str, str] | None = None,
) -> dict[str, Any]:
    """Start the full RAVO loop over a continual harness mutation for `task`.

    The loop (inspect, plan, implement, evaluate, diagnose, repair) runs in
    the background; this returns `{"started": True, "runId": ...}` right away,
    or `{"started": False, "reason": ...}` when a run is already in progress.
    Progress is visible in the Agents View and via `status()`. Set
    `global_=True` to target the global (cross-session) harness store; omit
    for local (session-scoped). `max_rounds` and `max_repairs` cap the loop.
    Pass `arc_agi={"repo_dir": "/path/to/ARC-AGI-3-Agents", "game": "ls20"}`
    to evaluate candidates by playing a real ARC-AGI-3 game instead of the
    LLM judge: the proposal must then carry an `arcAgent` (a Python `Agent`
    subclass) and the deep score is the fraction of levels completed.
    """
    if not isinstance(task, str) or not task.strip():
        raise TypeError("task must be a non-empty str")
    if instructions is not None and not isinstance(instructions, str):
        raise TypeError(
            f"instructions must be str or None, got {type(instructions).__name__}"
        )
    if not isinstance(global_, bool):
        raise TypeError(f"global_ must be bool, got {type(global_).__name__}")
    _check_count("max_rounds", max_rounds)
    _check_count("max_repairs", max_repairs)
    if arc_agi is not None:
        if not isinstance(arc_agi, dict) or not isinstance(arc_agi.get("repo_dir"), str) or not isinstance(arc_agi.get("game"), str):
            raise TypeError('arc_agi must be {"repo_dir": str, "game": str} or None')
    payload: dict[str, Any] = {"task": task}
    if instructions is not None:
        payload["instructions"] = instructions
    if global_:
        payload["global"] = True
    if max_rounds is not None:
        payload["max_rounds"] = max_rounds
    if max_repairs is not None:
        payload["max_repairs"] = max_repairs
    if arc_agi is not None:
        payload["arc_agi"] = {"repo_dir": arc_agi["repo_dir"], "game": arc_agi["game"]}
    return await host_request("ravo.run", payload)


async def cancel() -> dict[str, Any]:
    """Request cancellation of the active RAVO run.

    Returns `{"cancelled": True}` when a run was cancelled, otherwise
    `{"cancelled": False}`.
    """
    return await host_request("ravo.cancel")
