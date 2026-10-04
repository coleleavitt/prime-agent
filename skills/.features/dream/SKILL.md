---
name: dream
description: Start the Dream-RSI loop (rollout, dream a no-worse exploration policy, redeploy) over a scored task from the Python REPL, or a controlled experiment against the fixed-exploration control; returns immediately, runs in the background, progress in the Agents View and via status(). Default is local and token-free; llm_proposer/llm_dreamer and the guided arms spend tokens.
---

# Dream-RSI

Dream-RSI grows a discovery tree over a scored task, freezes each tree into a
zero-cost replay simulator, and improves a typed, serializable exploration
policy: it rolls out with the default policy, dreams one or more candidate
policies that should score no worse on replay, and redeploys the chosen policy
in a fresh rollout. The implementation lives in the host (the same one behind
the user's `/dream` command); this skill is the kernel-side interface to it.
Call it directly from the Python REPL:

```python
await dream.status()
await dream.run("circle-packing")
await dream.run("python-speedup", iterations=2, seed=7)
await dream.run("circle-packing", llm_dreamer=True)  # spends tokens
await dream.experiment("sum-difference", rounds=4)  # dream vs fixed, local
await dream.experiment("python-speedup", rounds=5, arms=("dream", "fixed", "dream-guided"), llm_proposer=True)  # spends tokens
await dream.experiment("autocorrelation", rounds=4, seeds=(7, 8, 9), llm_proposer=True, llm_dreamer=True)  # 3 independent seeds, one run
await dream.experiment("autocorrelation", rounds=4, priming="diverse", model="anthropic/claude-sonnet-5", thinking="off", max_output_tokens=4096, llm_proposer=True)
await dream.cancel()
```

## API

- `await dream.run(task, n=None, seed=None, workers=None, k1=None, k2=None,
  dreams=None, iterations=None, llm_proposer=False, llm_dreamer=False,
  model=None, thinking=None, max_output_tokens=None, priming=None)` — start
  a run. Returns `{"started": True, "runId": ...}` immediately, or
  `{"started": False, "reason": ...}` when a run is already in progress or
  Dream-RSI is not available in this session. `task` must be one of
  `circle-packing`, `sum-difference`, `python-speedup`, `autocorrelation`
  (the host's `DREAM_TASK_IDS`). `n` is the task size
  (e.g. circle count) when the task takes one. `seed` seeds the deterministic
  RNG. `workers`, `k1`, `k2`, `dreams`, `iterations` size the search. With
  `llm_proposer=True` each generation attempt is produced by a child agent, and
  with `llm_dreamer=True` each dreaming step's candidate policies come from a
  child agent; both spend tokens. The default (both False) runs the local
  zero-token proposer and dreamer and touches no network. `model`
  (`provider/id`), `thinking` (default `off`; the measured regime, see
  `docs/dream-rsi.md`) and `max_output_tokens` (the child's visible-answer
  cap; defaults per role: proposer 4096, 8192 for python-speedup, dreamer 4096,
  guidance 2048) apply to every child the run spawns. `priming="diverse"`
  additionally rolls out two fixed policies at iteration 0 so the frozen pool
  has replay support the default policy would never create; their probes are
  charged to round 1. `priming="none"` (the default) is byte-identical to
  leaving it out.
- `await dream.experiment(task, rounds=None, arms=("dream", "fixed"), n=None,
  seed=None, workers=None, k1=None, k2=None, dreams=None, llm_proposer=False,
  llm_dreamer=False, seeds=None, model=None, thinking=None,
  max_output_tokens=None, priming=None)` — start the paper's controlled
  comparison. Every arm
  starts from the same hand-written policy, seed and per-round budget and
  grows its own pool; the `fixed` arm (Recursive Fixed Exploration) never
  dreams, so round 1 is identical by construction and every later difference
  is the learned policy's. `rounds` is rollouts per arm (default 4). `arms` is
  a non-empty sequence of distinct names out of `dream`, `fixed`,
  `dream-guided`, `fixed-guided`; the guided arms prefix the proposer prompt
  with prior-trajectory insights (the semantic-guidance ablation) and need
  `llm_proposer=True`. Every non-local arm spends tokens. Returns
  `{"started": True, "runId": ...}` immediately; the result lands at
  `<dream dir>/experiments/<id>/result.json` (schema
  `prime-agent.dream.experiment/1`: per-round rows per arm, the headline
  multipliers against `fixed`), which `evals/dream/plot_experiment.py` turns
  into the round-best, compute and attempts plots. `status()` reports
  `resultPath` when it completes. `seeds` (1..16 distinct non-negative ints,
  exclusive with `seed`) runs one independent experiment per seed, sequentially
  under ONE run id and one progress stream, one `result.json` each under
  `experiments/<task>-s<seed>-...`; `status()` lists them in `resultPaths`
  (`resultPath` stays the last completed one). Seeds are independent
  replicates and are never pooled; pass every result path to the plotter for
  the noise floor. Every LLM seed costs a full experiment, so the count is
  capped at 16. `model`, `thinking`, `max_output_tokens` and `priming` are as in
  `run()`; priming trees are part of every arm's shared round 1.
- `await dream.status()` — current run status as a dict (`runId`, `phase`,
  `task`, `iteration`, `bestNodeScore`, `finalPolicyScore`, `improved`,
  `stopReason`, ...) or `{"phase": "idle"}` when nothing is running. An
  experiment adds `kind`, `experimentId`, `arm`, `armIndex`, `armCount`,
  `round`, `rounds`, `cumulativeProbes` and, on completion, `resultPath`; a
  multi-seed one adds `seed`, `seedIndex`, `seedCount`, `resultPaths` and, on
  the LLM path, `tokens` (the last completed seed's total).
- `await dream.cancel()` — request cancellation. Returns `{"cancelled": bool}`.

## Rules

- The run continues in the background; `run` never waits for it. Continue
  working normally and read `status()` when you need the outcome.
- Only one run per session at a time. Cancel or wait for the current run
  before starting another.
- A dreamed policy is DATA: it is parsed by a strict policy parser and scored
  under a no-regression rule, never executed. A worse or malformed policy can
  never replace the deployed one.
- Only the `llm_proposer` / `llm_dreamer` paths (and therefore the guided
  experiment arms) spend tokens. The default is local and token-free.
- An experiment's headline compares each arm against `fixed` only where the
  comparison is defined; "not reached" / "not comparable" are honest answers,
  and a policy's own-pool replay score is never compared across arms.

## Architecture map

The Dream-RSI architecture and its three in-session invocation surfaces (this
kernel skill, the human `/dream` command, and the live `dream_run_update`
progress in the Agents View) are documented in `docs/dream-rsi.md` at the
repository root; `evals/dream/README.md` covers the plotter and what the
result fields mean.
