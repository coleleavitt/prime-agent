"""Prime Agent dream skill: the Dream-RSI loop over a scored task from the kernel.

The run executes host-side (the same implementation as /dream); these functions
are thin typed wrappers over the generic host bridge (`rlm.host_request`). They
only work inside the Prime Agent Python kernel.
"""

from __future__ import annotations

from typing import Any

from rlm import host_request

# Mirrors DREAM_TASK_IDS (core/dream/tasks/index.ts); the host re-validates.
_TASKS = ("circle-packing", "sum-difference", "python-speedup", "autocorrelation")
_ARMS = ("dream", "fixed", "dream-guided", "fixed-guided")
_THINKING = ("off", "minimal", "low", "medium", "high", "xhigh", "max")
_PRIMING = ("none", "diverse")
# Mirrors DREAM_MAX_SEEDS (core/dream/run-service.ts): every LLM seed is a full experiment's spend.
_MAX_SEEDS = 16


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


def _check_seeds(value: Any) -> list[int] | None:
    if value is None:
        return None
    if isinstance(value, (str, bytes)) or not isinstance(value, (list, tuple)) or len(value) == 0:
        raise TypeError(f"seeds must be a non-empty list or tuple of ints, got {value!r}")
    if len(value) > _MAX_SEEDS:
        raise TypeError(f"seeds must list at most {_MAX_SEEDS} seeds, got {len(value)}")
    seeds: list[int] = []
    for seed in value:
        if isinstance(seed, bool) or not isinstance(seed, int) or seed < 0:
            raise TypeError(f"seeds must be non-negative ints, got {seed!r}")
        if seed in seeds:
            raise TypeError(f"seeds must be distinct, {seed!r} repeats")
        seeds.append(seed)
    return seeds


def _child_options(
    model: str | None, thinking: str | None, max_output_tokens: int | None, priming: str | None
) -> dict[str, Any]:
    """Validate the child-agent knobs shared by run() and experiment() into payload keys."""
    payload: dict[str, Any] = {}
    if model is not None:
        if not isinstance(model, str) or not model.strip():
            raise TypeError(f"model must be a non-empty 'provider/id' string or None, got {model!r}")
        payload["model"] = model.strip()
    if thinking is not None:
        if not isinstance(thinking, str) or thinking.strip().lower() not in _THINKING:
            raise TypeError(f"thinking must be one of {', '.join(_THINKING)} or None, got {thinking!r}")
        payload["thinking"] = thinking.strip().lower()
    _check_count("max_output_tokens", max_output_tokens)
    if max_output_tokens is not None:
        payload["max_output_tokens"] = max_output_tokens
    if priming is not None:
        if priming not in _PRIMING:
            raise TypeError(f"priming must be one of {', '.join(_PRIMING)} or None, got {priming!r}")
        if priming != "none":
            payload["priming"] = priming
    return payload


async def status() -> dict[str, Any]:
    """Read the current Dream-RSI run status.

    Returns the run status dict (`runId`, `phase`, `task`, `iteration`,
    `bestNodeScore`, `finalPolicyScore`, `improved`, `stopReason`, ...) or
    `{"phase": "idle"}` when no run is active. An experiment additionally
    reports `kind="experiment"`, `experimentId`, `arm`, `armIndex`, `armCount`,
    `round`, `rounds`, `cumulativeProbes` and, once complete, `resultPath`. A
    multi-seed experiment adds `seed`, `seedIndex`, `seedCount`, `resultPaths`
    (every completed seed's result.json) and, on the LLM path, `tokens` (the
    last completed seed's total).
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
    model: str | None = None,
    thinking: str | None = None,
    max_output_tokens: int | None = None,
    priming: str | None = None,
) -> dict[str, Any]:
    """Start the Dream-RSI loop over a scored task.

    The loop (rollout, dream a no-worse exploration policy, redeploy) runs in the
    background; this returns `{"started": True, "runId": ...}` right away, or
    `{"started": False, "reason": ...}` when a run is already in progress.
    Progress is visible in the Agents View and via `status()`. `task` must be one
    of circle-packing, sum-difference, python-speedup, autocorrelation. The
    default is the local zero-token proposer and dreamer; set `llm_proposer=True`
    or `llm_dreamer=True` to spend tokens on a child-agent proposer/dreamer.
    `model` ('provider/id'), `thinking` (default off) and `max_output_tokens`
    (the child's visible-answer cap; per-role defaults otherwise) apply to the
    proposer, dreamer and guidance children. `priming="diverse"` rolls out two
    extra fixed policies at iteration 0 so the pool has replay support the
    default policy would not create; charged to round 1.
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
    payload.update(_child_options(model, thinking, max_output_tokens, priming))
    return await host_request("dream.run", payload)


async def experiment(
    task: str,
    rounds: int | None = None,
    arms: Any = ("dream", "fixed"),
    n: int | None = None,
    seed: int | None = None,
    workers: int | None = None,
    k1: int | None = None,
    k2: int | None = None,
    dreams: int | None = None,
    llm_proposer: bool = False,
    llm_dreamer: bool = False,
    seeds: Any = None,
    model: str | None = None,
    thinking: str | None = None,
    max_output_tokens: int | None = None,
    priming: str | None = None,
) -> dict[str, Any]:
    """Start a controlled Dream-RSI experiment: the dreaming arm against the
    fixed-exploration control.

    Every arm starts from the same hand-written policy, seed and per-round
    budget and grows its own pool under `<dream dir>/experiments/<id>/<arm>`;
    the `fixed` arm never dreams (the paper's Recursive Fixed Exploration), so
    round 1 is identical by construction. `rounds` is the number of rollouts
    per arm (default 4). `arms` is a non-empty sequence of distinct names out
    of dream, fixed, dream-guided, fixed-guided; the guided arms inject prior-
    trajectory insights into the proposer prompt (the paper's semantic-guidance
    ablation) and need `llm_proposer=True`. Every arm spends tokens when
    `llm_proposer` or `llm_dreamer` is set; the default is local and
    token-free. Returns `{"started": True, "runId": ...}` right away, or
    `{"started": False, "reason": ...}` when a run is already in progress.
    `status()` reports `arm`, `round`, `cumulativeProbes` while it runs and
    `resultPath` (`.../experiments/<id>/result.json`, the input to
    `evals/dream/plot_experiment.py`) once it completes. `seeds` (a list of
    1..16 distinct non-negative ints, exclusive with `seed`) runs one
    independent experiment per seed sequentially under one run id, one
    result.json each (`experiments/<task>-s<seed>-...`; `status()` lists them
    in `resultPaths`); pass all of them to the plotter for a noise floor.
    `model`, `thinking` (default off), `max_output_tokens` and `priming` are as
    in `run()`; priming trees are part of every arm's shared round 1.
    """
    if task not in _TASKS:
        raise TypeError(f"task must be one of {', '.join(_TASKS)}, got {task!r}")
    _check_count("rounds", rounds)
    if (
        isinstance(arms, (str, bytes))
        or not isinstance(arms, (list, tuple))
        or len(arms) == 0
    ):
        raise TypeError(
            f"arms must be a non-empty list or tuple of arm names, got {arms!r}"
        )
    arm_list: list[str] = []
    for arm in arms:
        if arm not in _ARMS:
            raise TypeError(f"arms must be out of {', '.join(_ARMS)}, got {arm!r}")
        if arm in arm_list:
            raise TypeError(f"arms must be distinct, {arm!r} repeats")
        arm_list.append(arm)
    _check_count("n", n)
    _check_seed(seed)
    seed_list = _check_seeds(seeds)
    if seed is not None and seed_list is not None:
        raise TypeError("pass either seed or seeds, not both")
    _check_count("workers", workers)
    _check_count("k1", k1)
    _check_count("k2", k2)
    _check_count("dreams", dreams)
    if not isinstance(llm_proposer, bool):
        raise TypeError(f"llm_proposer must be bool, got {type(llm_proposer).__name__}")
    if not isinstance(llm_dreamer, bool):
        raise TypeError(f"llm_dreamer must be bool, got {type(llm_dreamer).__name__}")
    if any(arm.endswith("-guided") for arm in arm_list) and not llm_proposer:
        raise TypeError("dream-guided/fixed-guided require llm_proposer=True")
    payload: dict[str, Any] = {"task": task, "arms": arm_list}
    if rounds is not None:
        payload["rounds"] = rounds
    if n is not None:
        payload["n"] = n
    if seed is not None:
        payload["seed"] = seed
    if seed_list is not None:
        payload["seeds"] = seed_list
    if workers is not None:
        payload["workers"] = workers
    if k1 is not None:
        payload["k1"] = k1
    if k2 is not None:
        payload["k2"] = k2
    if dreams is not None:
        payload["dreams"] = dreams
    if llm_proposer:
        payload["llm_proposer"] = True
    if llm_dreamer:
        payload["llm_dreamer"] = True
    payload.update(_child_options(model, thinking, max_output_tokens, priming))
    return await host_request("dream.experiment", payload)


async def cancel() -> dict[str, Any]:
    """Request cancellation of the active Dream-RSI run.

    Returns `{"cancelled": True}` when a run was cancelled, otherwise
    `{"cancelled": False}`.
    """
    return await host_request("dream.cancel")
