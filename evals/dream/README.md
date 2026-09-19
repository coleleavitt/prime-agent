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
| `compute.png` | 3b / 5 | cumulative best vs cumulative discovery compute per arm, with the fixed arm's final best `T` and the equal-budget line `B`; the compute axis is agent-generated calls (bold) with probes as the thin secondary series when the files record provenance, else probes, and the subtitle says which |
| `attempts.png` | 6b | evaluated attempts per round per arm; `Δ k/n` marks a policy change in k of n seeds (adaptivity) |
| `proposals.png` | validity | child proposer results per round per arm: accepted (arm colour) stacked with rejected by reason (grey), and the local fallbacks those rejections caused as the x-marked line; the words `not recorded` when the file predates origin tracking |
| `headline.png` | the multipliers | `X.XXx fewer calls (a vs b)` and `Y.YYx higher score at budget B (a vs b)` per arm, read the right way round below 1 (`1.20x MORE calls (72 vs 60)`, `1.03x LOWER score`), or the literal words `not reached` / `not comparable`; then each arm's provenance totals (`47 probes = 2 agent-generated + 45 local (45 fallbacks)`) |
| `report.html` | all five | self-contained page with captions built from the result metadata and the per-round table, provenance columns included |

Several files are treated as seeds of one experiment (same task, rounds, budget, replay objective
and arms; distinct seeds) and reduced to mean / min / max per round, with the median of the defined
multipliers and "reached T in k/n seeds". A single file is plotted as is. Files that disagree are
refused, and the message names the field and both values, e.g.
`s2/result.json: objective differ from s1/result.json (objective beta1=0.05 beta2=0.05 vs beta1=0.01 beta2=0.02); plot one experiment at a time`:
seeds scored by different `beta1`/`beta2` are not one experiment, and a file that records no
objective does not pool with one that does.

## What the numbers mean

- **Probes** = evaluated attempts (revealed non-root nodes) on every path. On the LLM path a
  probe's candidate is either **agent-generated** (the child's output parsed and entered the
  tree, `origin: "llm"`) or a **local fallback** (the child's output was rejected and the local
  mutator stood in; the child's tokens were still spent, and sit on the fallback node).
- **Compute axis.** When every arm record carries provenance and a child proposer ran, the
  compute figure's bold series is `cumulativeAgentGeneratedCalls`, the paper's "agent calls",
  and `cumulativeProbes` is the thin secondary series (B is drawn in probes). Otherwise, and on
  the local path (0 LLM proposals: every probe is a local candidate by design, not a fallback),
  the axis is `cumulativeProbes` and the subtitle says why. The headline multipliers are on
  probes on every path. Handler invocations (proposer, dreamer, guidance) and child tokens are
  **cost**; they are in the table and never on that axis.
- **Provenance** (`agentGeneratedCalls`, `localFallbacks`, `llmProposals`, `llmAccepted`,
  `llmRejected` by reason: `parse`, `shape`, `invalid-candidate`, `error`, `length`, `aborted`,
  `turn-limit`, `budget`) is read per round and from `totals` when present, summed from the
  rounds when `totals` lacks it. A file written before origin tracking has none: that is
  **not recorded**, never 0; its cells read `-`, the validity panel prints the words, and a
  legacy seed pooled with a newer one is left out of the provenance means (the page says
  `recorded in k/n`). `--check` prints the per-round provenance table, each arm's totals as
  `47 probes = 2 agent-generated + 45 local (45 fallbacks); 47 LLM proposals = 2 accepted + 45
  rejected (parse 34, shape 11)`, and whether the tally adds up. On the card that line is
  warn-toned when the fallbacks outnumber the agent's candidates and bad when the tally does
  not add up.
- **T** = the fixed arm's final cumulative best; **B** = `equalBudget`, the smallest arm total
  (the caption says what the files recorded).
- **X x fewer calls** = `probesToTarget(fixed) / probesToTarget(arm)`, where `probesToTarget` is
  the compute at the FIRST round whose cumulative best reaches `T`. `not reached` when the arm
  never gets there.
- **Y x higher score** = `bestAtBudget(arm) / bestAtBudget(fixed)`, the cumulative best at the last
  round that fits inside `B`. `not comparable` when the arm's first round already exceeds `B`;
  when the reference best is 0 the ratio is undefined and the delta at budget is printed.
- **Below 1 the words turn round, the numbers do not.** A ratio below 1 is never written as
  `0.83x fewer calls`; it is written as its inverse with the direction spelled out,
  `1.20x MORE calls (72 vs 60)` / `1.03x LOWER score at budget B (a vs b)`, with the operands still
  in (arm vs reference) order. The colour follows the raw ratio (below 1 is bad). Across seeds an
  aggregate below 1 prints the inverse of the median ratio (`median 1.50x MORE calls`).
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
defined in 1 of 3, 2 of 3, all, and no seeds), the below-1 wording (`1.20x MORE calls (72 vs 60)`
and no `0.83x fewer` anywhere on the page), the refusal to pool files with different objectives, a
headline naming a reference arm that did not run, a file whose optional fields are all
malformed, and the provenance cases (a file carrying the per-round provenance fields, a legacy
file without them, the two pooled, a local-path file recording all zeros, an unlisted reject
reason, a tally that does not add up, and malformed provenance fields); the render tests are
skipped, not failed, where matplotlib is missing. `basedpyright evals/dream/plot_experiment.py`
reports no errors when run with an interpreter that has matplotlib
(`--pythonpath ~/Documents/AISpecies/.venv/bin/python`). The files are formatted with
`ruff format --line-length 120`.
