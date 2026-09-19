# evals/dream — the Dream-RSI evidence figures

`prime-agent dream experiment` runs the Dream-RSI loop against the paper's control, **Recursive
Fixed Exploration**: the identical agent, evaluator, initial policy, seed and per-round budget, with
an exploration policy that never changes. This directory turns one such run into the paper's
evidence figures (its Figures 3–6) for our fork. Nothing here is illustrative: every series is read
from a `result.json` the runner wrote.

## Produce a result

Local, zero tokens, deterministic in the seed and clock:

```sh
prime-agent dream experiment --task circle-packing --rounds 4 --seeds 1,2,3 --dir /tmp/dream-evidence
prime-agent dream experiment --task python-speedup --rounds 5 --seed 1 --json
```

In-session, with the LLM proposer (spends tokens; needed for the guided ablation arms):

```
/dream experiment --task python-speedup --rounds 4 --arms dream,fixed,dream-guided --llm-proposer
```

or from the kernel:

```python
await dream.experiment("python-speedup", rounds=4, arms=("dream", "fixed"), llm_proposer=True)
```

Each experiment writes `<dream dir>/experiments/<experimentId>/result.json` (schema
`prime-agent.dream.experiment/1`) with one tree store per arm under
`<dream dir>/experiments/<experimentId>/<arm>/trees/`. The dream dir is `$PRIME_AGENT_DREAM_DIR`
or `<agent dir>/dream`; `--dir` overrides it. The normal `<dream dir>/trees` pool is never touched.

## Plot it

```sh
python3 evals/dream/plot_experiment.py <dream dir>/experiments/<id>/result.json [--out <dir>]
python3 evals/dream/plot_experiment.py exp-s1/result.json exp-s2/result.json exp-s3/result.json   # seeds of one experiment
python3 evals/dream/plot_experiment.py <result.json> --check                                        # tables only, no matplotlib
```

The system `python3` on this machine has no matplotlib. The script detects that, looks for an
interpreter that has it (`$DREAM_PLOT_PYTHON`, then `~/Documents/AISpecies/.venv/bin/python`),
prints which one it is re-executing under, and continues there; with `--no-reexec` or no usable
fallback it exits 3 with the interpreter to try. `--check` needs only the stdlib.

Output (default `plots/` next to the first result file):

| file | paper figure | what it shows |
|---|---|---|
| `round_best.png` | 6a | round-best points per seed and the cumulative-best step per arm vs round |
| `compute.png` | 3b / 5 | cumulative best vs cumulative discovery-agent calls per arm, with the fixed arm's final best `T` and the equal-budget line `B` |
| `attempts.png` | 6b | evaluated attempts per round per arm; `Δ k/n` marks a policy change in k of n seeds (adaptivity) |
| `headline.png` | the multipliers | `X.XXx fewer calls (a vs b)` and `Y.YYx higher score at budget B (a vs b)` per arm, or the literal words `not reached` / `not comparable` |
| `report.html` | all four | self-contained page with captions built from the result metadata and the per-round table |

Several files are treated as seeds of one experiment (same task, rounds, budget and arms; distinct
seeds) and reduced to mean / min / max per round, with the median of the defined multipliers and
"reached T in k/n seeds". A single file is plotted as is.

## What the numbers mean

- **Compute axis** = `cumulativeProbes`: evaluated attempts (revealed non-root nodes), the
  discovery-agent calls, on every path. Handler invocations (proposer, dreamer, guidance) and
  child tokens are **cost**; they are in the table and never on that axis.
- **T** = the fixed arm's final cumulative best; **B** = `equalBudget`, the smallest arm total
  (the caption says what the files recorded).
- **X x fewer calls** = `probesToTarget(fixed) / probesToTarget(arm)`, where `probesToTarget` is
  the compute at the FIRST round whose cumulative best reaches `T`. `not reached` when the arm
  never gets there; below 1 is reported as is.
- **Y x higher score** = `bestAtBudget(arm) / bestAtBudget(fixed)`, the cumulative best at the last
  round that fits inside `B`. `not comparable` when the arm's first round already exceeds `B`;
  when the reference best is 0 the ratio is undefined and the delta at budget is printed.
- **delta final best** = `deltaBest(arm) = finalBest(arm) - T`, printed with its sign.
- **Ablation**: for each (unguided, guided) pair that ran, guided minus unguided final best; the
  paper's claim (semantic guidance is worse) holds only when the sign is negative, and the sign is
  printed either way.
- `--check` recomputes the headline from the rounds with the file's own `T` and `B` and prints
  whether it agrees with the file.
- Ratios are full doubles in the file and 2 decimals plus the raw operands on the page; a ratio
  that is not 1 is never printed as `1.00x` (it gets 4 decimals instead). The policy score on an
  arm's own pool is an in-arm replay estimate and is never compared across arms.
- **Across seeds** the card aggregates a ratio over the seeds where it is defined and says how
  many: `reached T in 1/3 seeds; single-seed ratio 1.20x fewer calls (not a median)` is one seed's
  ratio, warn-toned, never a "median" in the success colour; `reached T in 2/3 seeds; median of
  the 2 defined ratios ...` is warn-toned too, or bad when that partial median is below 1 (the
  undefined seeds never reached `T`, or had nothing inside `B`, so they cannot rescue it); only a
  ratio defined in every seed is shown as a median in the colour its value earns; defined in no
  seed, the line says `no calls ratio is defined`.
- **Missing and malformed fields** never crash the plotter. A multiplier the file leaves `null`
  stays undefined all the way to the page. A headline whose reference arm did not run is no
  headline (there is no control). A sub-field that is not the object or number it should be
  (`probesToTarget: 5`, `target: "1.3"`, `budget: 3`, `notes: "x"`) is treated as absent: `T` and
  `B` fall back to the values recomputed from the rounds, ratios stay undefined, and `--check`
  reports `agrees with the file: NO` so the gap is visible rather than papered over.

## Caveats

- `python-speedup` scores are wall-clock ratios against one shared baseline: tree shapes and ids
  are deterministic, the scores are not byte-deterministic. The result carries that note and the
  report repeats it.
- The local proposer on `python-speedup` plateaus early (its only safe lever is the redundant
  `reps` constant, so speedups stall near 4x); a rising curve past that needs the LLM proposer.
- Every non-local arm spends tokens. Round 1 is shared across arms on the LLM path (rolled out once
  and copied), so it is identical by construction; on the local path each arm rolls it out and the
  tree files are byte-identical anyway.
- An experiment with fewer than three rounds is labelled too short to show a curve.

## Tests

```sh
python3 -m unittest evals/dream/test_plot_experiment.py                                  # data layer; render test skips
~/Documents/AISpecies/.venv/bin/python -m unittest evals/dream/test_plot_experiment.py   # renders the four PNGs and the report
```

The tests use a synthetic result set (three arms, three rounds, two seeds, including a `not
reached` and a `not comparable` case), plus fixtures for the across-seeds honesty rule (a ratio
defined in 1 of 3, 2 of 3, all, and no seeds), a headline naming a reference arm that did not run,
and a file whose optional fields are all malformed; the render tests are skipped, not failed, where
matplotlib is missing. `basedpyright evals/dream/plot_experiment.py` reports no errors when run
with an interpreter that has matplotlib (`--pythonpath ~/Documents/AISpecies/.venv/bin/python`).
The files are formatted with `ruff format --line-length 120`.
