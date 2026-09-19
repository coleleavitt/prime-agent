"""Prime Agent dream skill: the Dream-RSI loop over a scored task from the kernel.

The run executes host-side (the same implementation as /dream); these functions
are thin typed wrappers over the generic host bridge (`rlm.host_request`). They
only work inside the Prime Agent Python kernel.
"""

from __future__ import annotations

from typing import Any

from rlm import host_request

_TASKS = ("circle-packing", "sum-difference", "python-speedup")


def _check_count(name: str, value: int | None) -> None:
    if value is None:
        return
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        raise TypeError(f"{name} must be a positive int or None, got {value!r}")


def _check_seed(value: int | None) -> None:
    if value is None:
        return
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise TypeError(f"seed must be a non-negative int or None, got {value!r}")


async def status() -> dict[str, Any]:
    """Read the current Dream-RSI run status.

    Returns the run status dict (`runId`, `phase`, `task`, `iteration`,
    `bestNodeScore`, `finalPolicyScore`, `improved`, `stopReason`, ...) or
    `{"phase": "idle"}` when no run is active.
    """
    return await host_request("dream.status")


async def run(
    task: str,
    n: int | None = None,
    seed: int | None = None,
    workers: int | None = None,
    k1: int | None = None,
    k2: int | None = None,
    dreams: int | None = None,
    iterations: int | None = None,
    llm_proposer: bool = False,
    llm_dreamer: bool = False,
) -> dict[str, Any]:
    """Start the Dream-RSI loop over a scored task.

    The loop (rollout, dream a no-worse exploration policy, redeploy) runs in the
    background; this returns `{"started": True, "runId": ...}` right away, or
    `{"started": False, "reason": ...}` when a run is already in progress.
    Progress is visible in the Agents View and via `status()`. `task` must be one
    of circle-packing, sum-difference, python-speedup. The default is the local
    zero-token proposer and dreamer; set `llm_proposer=True` or `llm_dreamer=True`
    to spend tokens on a child-agent proposer/dreamer.
    """
    if task not in _TASKS:
        raise TypeError(f"task must be one of {', '.join(_TASKS)}, got {task!r}")
    _check_count("n", n)
    _check_seed(seed)
    _check_count("workers", workers)
    _check_count("k1", k1)
    _check_count("k2", k2)
    _check_count("dreams", dreams)
    _check_count("iterations", iterations)
    if not isinstance(llm_proposer, bool):
        raise TypeError(f"llm_proposer must be bool, got {type(llm_proposer).__name__}")
    if not isinstance(llm_dreamer, bool):
        raise TypeError(f"llm_dreamer must be bool, got {type(llm_dreamer).__name__}")
    payload: dict[str, Any] = {"task": task}
    if n is not None:
        payload["n"] = n
    if seed is not None:
        payload["seed"] = seed
    if workers is not None:
        payload["workers"] = workers
    if k1 is not None:
        payload["k1"] = k1
    if k2 is not None:
        payload["k2"] = k2
    if dreams is not None:
        payload["dreams"] = dreams
    if iterations is not None:
        payload["iterations"] = iterations
    if llm_proposer:
        payload["llm_proposer"] = True
    if llm_dreamer:
        payload["llm_dreamer"] = True
    return await host_request("dream.run", payload)


async def cancel() -> dict[str, Any]:
    """Request cancellation of the active Dream-RSI run.

    Returns `{"cancelled": True}` when a run was cancelled, otherwise
    `{"cancelled": False}`.
    """
    return await host_request("dream.cancel")
