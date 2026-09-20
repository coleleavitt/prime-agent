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
/dream experiment --task autocorrelation --rounds 4 --seeds 1,2,3 --llm-proposer --llm-dreamer
```

`--seeds` runs the seeds one after another under one run and writes one `result.json` per seed
(each seed is its own experiment directory); pass all of them to the plotter for the noise floor.
In-session the list is capped at 16 seeds, since every LLM seed is a full experiment's spend.

`--priming diverse` (CLI and `/dream`; `priming="diverse"` in the skill) rolls out the fixed
`PRIMING_DIVERSE` pair at round 1 in every arm and charges its probes to round 1 (`primingTreeIds`,
`primingProbes` on the row); `--beta3` sets the anytime weight next to `--beta1` / `--beta2`; an
in-session run also takes `--model`, `--thinking` and `--max-output-tokens` for the child agents
(defaults: thinking off; caps 4096 for the proposer, 8192 on python-speedup, 4096 dreamer, 2048
guidance), recorded on each arm's `mode`. Beside every arm's `trees/` the runner writes
`dreams/<runId>.jsonl` (one verdict per candidate per dreaming step, the arm's post-hoc final
selection as iteration `-1`) and, on the LLM path, `rejections/<runId>.jsonl`; `result.json`
carries the same verdicts as each round's `dreaming.candidateVerdicts`, which is what `dreaming.png`
reads. `docs/dream-rsi.md` defines every field.

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
| `attempts.png` | 6b | evaluated attempts per round per arm; `Δ k/n` marks a policy change in k of n seeds (adaptivity); a dreaming arm whose policy never changed is stamped `policy never changed in k/n seed(s): dreaming inert` |
| `proposals.png` | validity | child proposer results per round per arm: accepted (arm colour) stacked with rejected by reason (grey), and the local fallbacks those rejections caused as the x-marked line; the words `not recorded` when the file predates origin tracking |
| `dreaming.png` | the audit | one panel per arm that dreamed: every candidate's replay value per step (filled = eligible for the argmax, hollow = identical / duplicate / quality-rejected / unmeasurable; llm candidates in the arm colour, local grey), the incumbent as a tick, the chosen policy starred, the best lever-scan policy as a triangle, the words `improved` / `no change` with the lever gap, and a strip with the share of candidates replayed fully in support; a file written before the audit shows the incumbent and chosen values only and says so |
| `headline.png` | the multipliers | `X.XXx fewer calls (a vs b)` and `Y.YYx higher score at budget B (a vs b)` per arm, read the right way round below 1 (`1.20x MORE calls (72 vs 60)`, `1.03x LOWER score`), or the literal words `not reached` / `not comparable`; the exact (first-probe) calls line beside it; the noise floor across seeds, the paired per-seed deltas and the quality verdict; per arm the paired probes-to-T deltas, the efficiency verdict and the `spend (not a verdict)` line with best-at-B per seed; the dreaming summary; then each arm's provenance totals (`47 probes = 2 agent-generated + 45 local (45 fallbacks)`) |
| `report.html` | all six | self-contained page with captions built from the result metadata, the per-step dreaming table and the per-round table, provenance columns included |

Several files are treated as seeds of one experiment (same task, rounds, budget, replay objective
and arms; distinct seeds) and reduced to mean / min / max per round, with the median of the defined
multipliers and "reached T in k/n seeds". A single file is plotted as is. Files that disagree are
refused, and the message names the field and both values, e.g.
`s2/result.json: objective differ from s1/result.json (objective beta1=0.05 beta2=0.05 vs beta1=0.01 beta2=0.02); plot one experiment at a time`:
seeds scored by different `beta1`/`beta2`/`beta3` are not one experiment, a file that records no
objective does not pool with one that does, and a file without `beta3` (the two-term objective)
does not pool with one that has it.

Seeds are independent replicates. The pool a dreaming step replays grows across rounds within
one arm only, never across arms (the control never sees the dream arm's trees, and vice versa)
or seeds; that is what makes the paired per-seed comparison below valid, and there is no
cross-store pooling to switch on. The tree ids collide across arms of one seed on purpose:
round 1 is shared by construction, and the arms' stores are separate directories.

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
- **Exact headline.** A round record may carry `probesToRoundBest` (the probe, in reveal order,
  at which the rollout found its best node) and `improvements` (`[{probe, score}]`, the
  best-so-far curve at the probes where it rose); the headline may carry `probesToTargetExact`
  and `callsMultiplierExact`. The exact count is the compute at the FIRST PROBE whose score
  reaches `T`, not the end of the round that contains it, and it is printed beside the
  rollout-granular line as `exact 1.16x fewer calls (19 vs 22)`. The file's own numbers win;
  without them the count is recomputed from `improvements` when every arm has the curve; a file
  without either reads `exact probes to T not recorded` (not recorded is not `not reached`).
- **Noise floor and verdict.** The fixed arm's final best and probes-to-target vary from seed to
  seed with nothing but the proposer's sampling, so their spread across seeds (`min..max`,
  `spread`, sample `std`) is the floor any dream-vs-fixed difference has to clear. Per non-fixed
  arm the card lists the paired per-seed delta of final best (arm minus fixed; both arms share
  round 1 within a seed), its mean and sign counts, and one verdict: `single seed: no verdict`
  (one file: there is no floor to measure); `exceeds noise floor` only when the absolute mean
  paired delta is larger than the fixed arm's `min..max` spread AND every seed's delta has the
  same sign (ok-toned when positive, bad when negative); otherwise `within noise floor`. When the
  dreaming arm's policy never changed in every seed there was no treatment, and the verdict is
  forced to `within noise floor (dreaming inert)` whatever the numbers say; `attempts.png` carries
  the same stamp. With one seed the words add `(dreaming inert: the arms ran the same policy)` so
  the reader has both facts.
- **Efficiency verdict and spend.** The quality verdict says nothing about compute, and a dreaming
  arm's effect may be there, so each non-fixed arm gets a second, paired verdict
  (`efficiency_effects`) on probes-to-T, T being the fixed arm's final best. The card lists the
  paired per-seed delta of probes-to-T (arm minus fixed), rollout-granular and exact. The verdict
  reads the exact count when every file records it and the rollout-granular one otherwise, and the
  line says which: `efficiency verdict (exact probes to T): ...`. A seed in which the arm never
  reached T is written `not reached`; it is never clamped to the arm's total, imputed, or dropped
  from a mean (no mean is printed unless every seed has a delta). The rule: `single seed: no
  verdict` with one file; `no verdict: target not reached in k/n seeds` unless the arm reached T
  in EVERY seed, because an arm that spends less and never reaches the control's best has not
  demonstrated efficiency; `within noise floor (dreaming inert)` when the policy never changed in
  any seed; `exceeds noise floor` only when every seed's delta has the same sign (a delta of 0 is
  no sign) AND the absolute mean delta is larger than the fixed arm's own `min..max` spread of
  probes-to-T on the same basis (ok-toned when fewer probes, bad when more); otherwise `within
  noise floor`. Below it, the `spend (not a verdict)` line gives total probes per arm per seed,
  their paired delta, its mean and the ratio of the summed totals, and the next line says why it
  is not a verdict: fewer total probes is only an efficiency gain if quality at equal compute is
  not lower, citing `bestAtBudget` of the arm and of the fixed arm per seed and in how many seeds
  the arm is lower (warn-toned when any). A file without a fixed arm gets none of these lines.
- **Dreaming audit.** A round's `dreaming` block (the step that chose that round's policy) has
  always carried `currentScore`, `chosenScore`, `improved` and `candidates` (a count). It may now
  also carry `candidateVerdicts` (one record per candidate: `value`, `quality`, `anytime`, `cost`,
  `roundsSaved`, `N`, `rounds`, `outOfSupportCells`, `inSupportMean`, `inSupportMin`, `origin`
  `llm`/`local`, `changed`, `duplicateOf`, `eligible` and the `reason` it won, tied, lost, was
  quality-rejected, unmeasurable, identical to the incumbent or a duplicate), `dreamer`
  (`llm`/`local`/`mixed`) and `leverScan` (`policies`, `eligible`, `bestValue`, `bestPolicyId`,
  `gap`: the best a fixed grid of local policies reached on the same pool, independent of what the
  dreamer proposed; a gap of 0 means the pool had no lever, whatever was proposed). The card prints
  per arm `dreaming ran k step(s), accepted a candidate in i, policy changes c` and the inert flag;
  `--check` prints a per-step table (dreamer, candidates, eligible, incumbent, chosen, improved,
  lever gap, share in support); `dreaming.png` draws it. A file written before the audit reads
  `per-candidate scores not recorded (result predates the audit)` and its panel shows the
  incumbent and chosen values only.
- **Other optional fields** (`stoppedEarly` per arm, else derived from `decisionRounds < k1` when
  every round recorded it; `primingTreeIds` / `primingProbes` on round 1; `mode.thinking` /
  `mode.maxOutputTokens`; `initialPolicy.beta`; a note when `k1 <= beta` so patience can never
  stop a rollout before the round cap) are read when present. Absent is "not recorded", never 0,
  and an older file plots exactly as before.
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
~/Documents/AISpecies/.venv/bin/python -m unittest evals/dream/test_plot_experiment.py   # renders the six PNGs and the report
```

The tests use a synthetic result set (three arms, three rounds, two seeds, including a `not
reached` and a `not comparable` case), plus fixtures for the across-seeds honesty rule (a ratio
defined in 1 of 3, 2 of 3, all, and no seeds), the below-1 wording (`1.20x MORE calls (72 vs 60)`
and no `0.83x fewer` anywhere on the page), the refusal to pool files with different objectives, a
headline naming a reference arm that did not run, a file whose optional fields are all
malformed, and the provenance cases (a file carrying the per-round provenance fields, a legacy
file without them, the two pooled, a local-path file recording all zeros, an unlisted reject
reason, a tally that does not add up, and malformed provenance fields), the noise-floor verdict
(one seed, exceeds, mixed signs, a mean inside the spread, negative, inert in every seed and in
one seed), the efficiency verdict (one seed, target not reached in one of three seeds, one
sign with a mean above and inside the fixed spread, more probes in every seed, mixed signs, a zero
delta, exact vs rollout-granular selection, not reached on the exact basis, inert, no reference
arm), the dreaming audit (verdicts, dreamer and lever scan read; an older file's fallback;
malformed entries; pooled with an older seed), the exact headline (recomputed from
`improvements`, the file's numbers winning, not recorded vs not reached, the exact noise floor)
and the other optional fields; the render tests are skipped, not failed, where matplotlib is
missing. `basedpyright evals/dream/plot_experiment.py`
reports no errors when run with an interpreter that has matplotlib
(`--pythonpath ~/Documents/AISpecies/.venv/bin/python`). The files are formatted with
`ruff format --line-length 120`.
