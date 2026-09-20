# Dream-RSI

`prime-agent dream` is a self-contained implementation of DREAM-RSI (Zheng et al., 2026): a
recursive self-improvement loop that grows a **discovery tree** over a scored task, freezes each
tree into a **zero-cost replay simulator**, and then **dreams** — searching over a typed,
serializable exploration policy and redeploying the improved one to grow the tree pool further.

The default path (`dream loop` on `circle-packing`) runs the whole cycle with a deterministic local
proposer and a local policy search. It spends **zero model tokens** and touches **no network**. The
LLM proposer and LLM dreamer are separate, flag-gated options that require an in-session agent
handler; the standalone CLI rejects them.

Everything the CLI needs lives under `packages/coding-agent/src/core/dream/`. The CLI module
(`src/cli/dream-command.ts`) only parses argv, resolves a seed, a clock, a store directory and a
task, and calls into `core/dream`.

## The three stages

### 1. Online explore (grow the tree)

A fixed exploration policy guides a fixed discovery agent to grow a discovery tree `T`.

- The **root** `r` is the initial workspace for the task.
- Each non-root node `v` has exactly one parent (the node the attempt resumed from) and records the
  attempt's artifact, its evaluation diagnostics, and a numeric score `s_v` (higher is better).
- The **eligible set** `A(T) = {r} ∪ {leaves of T}`. Because a node leaves `A(T)` the moment it
  gains a child, every non-root node ends with at most one child while the root accumulates many —
  exactly the shape replay needs.
- With `W` parallel workers, one round's action is a batch `C ⊆ A(T)`, `|C| ≤ W`: each chosen node
  is a starting point for one new generate+evaluate attempt. The rollout ends on an empty batch,
  after `K1` rounds, or when the policy's stop rule fires.
- A round **improves** the best score only when it beats it by more than `IMPROVE_EPS = 1e-12`
  (`interpreter.ts`, shared by both online drivers and by replay), which is what the `patience`
  stop rule counts.
- The rollout also records its **best-so-far curve**: `ExploreResult.probesToBest` is the `seq`
  (reveal order) of the best valid node, 0 when the root is best, and `improvements` is
  `[{probe, score}]` at every probe where the running best rose (the root at probe 0 when valid).
  The experiment's exact headline is built from these.

### 2. Replay simulator (freeze the tree, zero execution)

A recorded tree is a frozen simulator. An alternative policy re-walks it: each round it picks a
batch of eligible nodes and replay **deterministically** returns the recorded child of each selected
node — the earliest-created not-yet-revealed child for the root, the unique unrevealed recorded child
for a non-root leaf. Nothing new is generated, so thousands of policies are scored at zero cost.

A legally selected cell with **no** unrevealed recorded child (the root once all its children are
revealed, or a non-root leaf that never had a recorded child) is **out of support**: it reveals
nothing but is **charged as a probe**, because online it would have been one. `ReplayResult`
carries the whole accounting (`replay.ts`):

- `N` revealed non-root nodes and `outOfSupportCells` (per cell, not per round);
- `selectedCells = N + outOfSupportCells`, the charged selections, and
  `inSupport = N / selectedCells` (1 when nothing was selected);
- `bestSoFar`, the running best valid score after each charged selection (length
  `selectedCells`; an out-of-support selection repeats the running best; 0 while nothing is valid);
- `probesToBest`, the 1-based index of the first charged selection at which `bestScore` was
  reached (0 when the root is best);
- `rounds`, the decision rounds taken, bounded by `k2`, the tree being exhausted, an empty batch
  or the stop rule.

Replay detects an improvement with the same `IMPROVE_EPS` as the online drivers, so the `patience`
rule fires on the same round online and in replay and the incumbent policy replays its own tree
exactly. That is now a structural guarantee, not an observation: before the shared constant replay
used a strict `>` and a tree with two scores within `1e-12` could have stopped on a different round.
A recorded tree containing such a near-tie replays differently once, under the shared rule.

### 3. Dreaming policy improvement

From the current policy `π⁰`, produce `M` revised policies. Each is scored on every recorded tree by
the replay objective, and the best — which always includes the current policy in the candidate set —
is redeployed online to expand the pool.

#### The replay objective

Every term is dimensionless (`core/dream/objective.ts`). With `N` the revealed non-root nodes,
`oos` the out-of-support cells, `S = N + oos` the charged selections, `B = W · k1` the per-rollout
probe budget and `rounds` the replay decision rounds:

```
q            = clamp((max_{v in revealed} s_v − poolMin) / (poolMax − poolMin), 0, 1)
q_p          = q of the running best after charged selection p          (p = 1..S)
anytime      = ( Σ_{p=1..S} q_p  +  max(0, B − S) · q ) / max(B, S)
cost         = S / B                                                     (not clamped)
roundsSaved  = 1 − rounds / k1                                           (not capped)
V            = (1 − beta3) · q  +  beta3 · anytime  −  beta1 · cost  +  beta2 · roundsSaved
```

- **quality** `q` — the best valid score, normalized against the **pool's** observed valid-score
  range (`poolScoreScale`: min and max over every valid node of every tree in the frozen pool,
  roots included). When the pool has a single score the term is 1 at or above it and 0 below.
- **anytime** — the mean normalized best-so-far over the budget, evaluated after every charged
  selection and held flat at `q` for the unspent tail. An out-of-support selection appends a repeat
  of the running best, so it counts as a probe that found nothing. `anytime` is in `[0, 1]` and
  never exceeds `q`; it rewards reaching the best EARLY, which is what the experiment headline
  (probes to a target) measures.
- **cost** — the charged fraction of the budget. Out-of-support cells are charged. It is not
  clamped: with `k2 > k1` a replay may charge more than one budget.
- **roundsSaved** — uncapped, so a replay that runs past `k1` (which `k2 > k1` allows) goes
  negative and pays for it.

`ObjectiveTerms = { quality, anytime, cost, roundsSaved, value }`. The defaults are
`DEFAULT_OBJECTIVE = { beta1: 0.05, beta2: 0.10, beta3: 0.25 }`: `beta2 > beta1` so saving one
round outweighs the at most `W` probes it could cost, and a quarter of the quality weight goes to
earliness. The ordering the objective encodes is therefore rounds first, then probes, then
earliness, with quality guarded separately (below). Bounds: `V ≤ 1 + beta2`, and the cost terms
move `V` by at most `beta1 · S/B + beta2 · |1 − rounds/k1|`. With `beta3 = beta2 = 0` the
objective reduces to the strictly-cost form `q − beta1 · S/B`. `--beta1` / `--beta2` / `--beta3`
(and `ReplayObjectiveConfig` on every core entry, where `beta3` is a required field; the CLI checks
`beta3` is in `[0, 1]` and every beta is finite and non-negative) change them, and the experiment
result records the values it ran with.

**What V cannot do (the out-of-support limit, stated for the record).** Replay reveals only what
the recording holds, so no candidate can score a higher quality than the recording contains: V
rewards "the same best for less" and can never reward "more". The quality guard is therefore blind
to quality a cheaper policy would LOSE online (its fewer probes reveal the recorded best because
the recording is there to be walked), every same-best-for-less winner is adopted, and once a
cheaper policy is deployed no exploring policy can re-win against it on the trees it grew (its
extra probes reveal nothing there, so it is `worse` at equal quality). Measured on the recorded
autocorrelation pools (W 3, k1 6, 10 incumbents, winners re-run online on 6 fresh seeds): 8
incumbents had a grid winner, every winner spent 2 to 6 fewer probes online, and 7 of the 8 had a
lower online best (by 2e-6 to 8e-4 on a best of about 0.5005); on circle-packing s7 the adopted
explore-root/fixed-rounds/beta 6 policy scored 1.042620 in 6 probes against the incumbent's
1.047197 in 12 on fresh seeds, and the recovery selection back to the incumbent reported `worse`.
The ratchet only turns towards spending less. This is a property of a frozen replay, not a knob;
it is why the selection scores only the trees the incumbent replays in full support (below), and
why the experiment headline (probes to a target on fresh rollouts), not V, is the measure of a run.

**Why the parallelism term is gone.** The previous normalized form was
`V = q − beta1 · N/(W · k1) + beta2 · N/(rounds · W)`, a probe cost against a "mean batch fill"
bonus. With `beta1 = beta2 = beta` it satisfies the identity

```
V − q = (beta / W) · N · (1 / rounds − 1 / k1)
```

which is exactly zero whenever `rounds == k1`, for EVERY `N`. On the recorded run 2
(autocorrelation `n = 64`, seed 7, `W = 3`, `k1 = 6`, `k2 = 12`, four dreaming rounds, Sonnet-5
proposer and dreamer) `k1` equalled `DEFAULT_POLICY.beta`, so the patience rule could never stop a
rollout before the round cap, all eight recorded trees ran exactly six decision rounds, every
incumbent replay scored `V == q` to the last digit, and of the 6549 policies reachable from the
grid none was both quality-eligible and `V`-better. Dreaming was inert for that reason alone: the
dreamer's twelve LLM candidates (four per step, none a fallback) all tied or were quality-rejected
(`dream.chosen_score == dream.current_score == dream.current_quality` on every `dream.dream` span),
both arms grew every tree with the initial policy, and the headline read `1.40x MORE calls`. Under
the new form `V` is strictly decreasing in `S` at every round count, so "the same best for fewer
probes" is a strict win.

**Break-even numbers under this `V`** (recomputed from the source; `test/dream-improve.test.ts`
pins them). On the recorded fixtures (`test/fixtures/dream`, circle-packing seed 7 rounds 1-2,
`W = 4`, `k1 = 12`, `k2 = 24`, `DEFAULT_OBJECTIVE`):

- the exploring policy `1be99d403b0405a3` scores `V 0.797390` (`q 0.878487`, `anytime 0.708267`,
  `cost 37/48 = 0.770833`, `roundsSaved 0`);
- the one-probe collapsed policy `f559ec93fc3b1773` scores `V 0.435252` (`q 0.344627`,
  `cost 1/48 = 0.020833`, `roundsSaved 11/12 = 0.916667`); they tie in `V` only at
  `beta1 = 0.533`, a 10.7x margin over 0.05, and the quality guard rejects the collapsed policy
  regardless;
- the chain policy (best-first, `batchSize 1`, stop `never`: 13 probes plus 11 out-of-support
  cells over 24 rounds) reaches the same quality with `cost 0.5` and `anytime 0.803087` but
  `roundsSaved −1`, so `V 0.734635`; it ties the exploring policy only when `beta2 ≤ 0.0372`, so
  0.10 keeps full batches preferred with a 2.7x margin.

On the run-2 autocorrelation dream pool (4 trees, `W = 3`, `k1 = 6`) the incumbent scores
`V 0.842156` (`q 0.883344`, `anytime 0.849147`, `cost 0.652778`, `roundsSaved 0`, mean `N` 11.75)
and the lever-scan grid (337 policies, 84 eligible) opens a gap of `+0.007804` with
weighted / fixed-rounds / `batchSize 2` (the same best, `N` 9, 6 rounds) — "the same best for fewer
probes", which the old form scored as an exact tie.

**History.** The paper tunes `beta1` per domain because its penalty is on raw scores. This
codebase first shipped `V = best − 0.01 · N + 0.02 · N / rounds`: on circle-packing (pool range
0.48) a 36-probe rollout paid 0.36 while real score gains were about 0.1, and the dreamer
"improved" by learning to stop exploring (seed 7, dream-arm probes 36, 12, 1, 1, 4, 1 with the best
frozen at round 1 against the fixed control's 36, 38, 38, 38, 37, 32). Normalizing `q` to the pool
range and the cost to budget fractions fixed the scale; the parallelism bonus then hid the
cancellation above until run 2 exposed it. `test/dream-improve.test.ts` keeps the raw formula as a
literal and pins the recorded collapse regression on two of the recorded trees.

#### Selection, verdicts and the lever scan

A policy's pool score (`PoolScore`, `improve.ts`) is the arithmetic mean of every term over the
recorded trees in a deterministic order (sorted by tree id): `value`, `quality`, `anytime`, `cost`,
`roundsSaved`, plus the mean `N`, `rounds` and `outOfSupportCells` and the pool's `inSupportMean`
and `inSupportMin` (`ReplayResult.inSupport` averaged and at its minimum; both 1 for an empty pool).

`selectBestPolicy(current, candidates, pool, cfg)` first splits the pool into the **measured pool**
(`measurePool`: the trees the current policy replays with zero out-of-support cells) and the rest,
at the cost of one replay per tree. A tree another policy grew — a priming tree, or a tree from
before a policy change — on which the incumbent's replay reveals almost nothing and burns dead
rounds is not a baseline: comparing a candidate against it credits fictional quality gains that can
mask a real regression on the incumbent's own trees, or hands the win to a candidate that merely
burns fewer dead rounds there while behaving identically online (on the real autocorrelation
priming tree the incumbent replayed N 1, 6 out-of-support cells, 7 rounds, `roundsSaved` −0.167,
and a patience-3 twin "won" by +0.019 from that tree alone). Every score below is a mean over the
measured trees only, normalized to the measured pool's scale; the result is identical to calling
the selection on the measured trees alone, and `PolicySelection` reports `poolSize`,
`measuredTrees` and `currentInSupport` (the incumbent's mean in-support share over the whole
pool). When no tree is measured nothing is eligible and every simulated candidate is
`unmeasurable`. Only the incumbent's support decides membership; a candidate off support on a
measured tree is charged for it as before.

It then scores `{current} ∪ candidates` and returns the
argmax over the **eligible** entries: the current policy, plus every simulated candidate whose mean
`quality` is at least the current policy's minus `qualityEps` (default 0) within `SELECT_EPS`
(`1e-9`). Ties in `V` resolve to the current policy, then to the lowest policy id; `improved` is
true only when an eligible candidate strictly beats current in `V`. Because `V` is a pure function
of the frozen history, its scale and the budget, the chosen policy is provably **no worse than the
current one on replay in `V` and never lower in quality**: a candidate cannot win by collapsing
exploration, only by reaching at least the same best in fewer rounds, with fewer charged probes, or
earlier. It is an off-policy estimate: a policy that would explore un-recorded branches is out of
support there.

Every candidate gets a `CandidateVerdict` (`types.ts`), returned as `PolicySelection.candidates`
in input order:

```
{ index, policyId, policy, origin: "llm" | "local", changed: string[], duplicateOf: number | null,
  value, quality, anytime, cost, roundsSaved, N, rounds, outOfSupportCells, inSupportMean,
  inSupportMin, eligible: boolean, reason }
```

`changed` lists the fields differing from the current policy in schema order
(`policyFieldsDiffering`). `reason` is one of `CANDIDATE_REASONS`, decided in this order:

| reason | when |
|---|---|
| `identical` | the candidate's id equals the current policy's; never simulated, never in the argmax |
| `duplicate` | the same id as an earlier candidate (`duplicateOf` is that index); scored once, excluded from the argmax and from `scoredCount` |
| `winner` | the chosen policy: eligible and a strict improvement |
| `unmeasurable` | not the winner and either differs from current only in `REPLAY_DEAD_FIELDS` (scored as current, without a simulation), or replayed out of support on some measured tree (`inSupportMin < 1`), or no tree was measurable: its replay is biased and says nothing about it. Takes precedence over `quality-rejected` |
| `quality-rejected` | fully in support, simulated, failed the quality guard |
| `tie` | eligible and within `SELECT_EPS` of the chosen value; lost the tie-break |
| `worse` | eligible, fully in support, below the chosen value |

`eligible` says whether the candidate entered the argmax (simulated and passed the quality guard);
an off-support candidate can therefore be `eligible: true` with `reason: "unmeasurable"`, which
means it competed but its number is not trusted. `scoredCount` is `1 +` the distinct,
non-identical candidates; `qualityRejected` counts exactly the `quality-rejected` verdicts (an
off-support quality failure is `unmeasurable` only, so the categories on `dream.dream` sum to the
candidate count); `simulations` is the exact number of `simulatePolicy` calls the selection made
(the current policy on every tree, then each simulated candidate on the measured trees; identical,
duplicate and replay-dead-only candidates cost none). `selectBestPolicy` accepts either bare `ExplorationPolicy[]` (origin
`local`) or `{ policy, origin }[]`, and `PolicySelection.dreamer` is `llm` when every candidate came
from a child agent, `local` when none did, else `mixed` (`dreamerKindOf`).

Every dreaming step also runs a **lever scan** (`runLeverScan`): a fixed, deterministic,
rng-free grid of local policies — the current policy, then every selection rule x every stop rule x
`batchSize` 1..min(W, 8) x `beta ∈ LEVER_SCAN_BETAS = {1, 2, 3, 4, 6, 8, 12}` at the current
policy's other numeric values, deduplicated by policy id — scored by the same `selectBestPolicy`
rule, on the same measured pool. `LeverScanRecord = { policies, eligible, bestValue,
bestPolicyId, gap, simulations }`, with
`gap = bestValue − currentValue` over the eligible grid policies and `gap 0` /
`bestPolicyId = current` when nothing beats current. It answers "did this pool have ANY lever?"
independently of what the dreamer proposed, so an inert step can be labelled "no lever on this
pool" rather than "the dreamer proposed nothing better". `DreamingOptions.leverScan: false` skips
it; it touches no rng, so trees are byte-identical with and without it.

`runDreaming` (`improve.ts`) returns `DreamResult` with `candidates: CandidateVerdict[]`, `dreamer`
and `leverScan` next to the chosen policy and scores. The loop copies these onto the round record:
`DreamRoundDreaming.candidates` STAYS the proposed count (the result schema is additive-only) and
the additive fields are `candidateVerdicts`, `dreamer`, `leverScan` and `measuredTrees`.

#### The local dreamer

`proposePolicies(current, m, rng)` draws `M` candidates, each from `rng.fork("cand:<i>")` so the
set is order-independent. A mutation flips the selection or stop rule (probability 0.34, never
`recoveryPolicy`) or perturbs one or two **replay-live** numeric fields (`batchSize`, `beta`,
`promisingThreshold`, `targetScore`, `explorationBias`) with a bounded gaussian and re-clamps. It
never returns the current policy's id: a mutation that clamps back onto it is retried on
`rng.fork("retry:<i>")` up to `MUTATION_RETRIES = 8` times, then the selection rule is advanced
deterministically. Because the replay-dead fields left the mutable set, the local path's candidate
ids differ from earlier releases; the recorded trees are unaffected, only the tests that pinned
candidate ids changed.

#### The dreams log

Every dreaming step is written to `<store dir>/dreams/<runId>.jsonl` (`dreams.ts`, mirroring
`rejections.ts`; directory 0700, file 0600, created on the first step): one line per candidate,
`{type:"candidate", ts, experimentId?, arm?, iteration, ...CandidateVerdict}`, then one step line
`{type:"step", ts, experimentId?, arm?, iteration, poolSize, measuredTrees, currentValue,
chosenPolicyId, improved, dreamer, leverScan}`. The arm-level post-hoc final selection (`selectBestPolicy` over
`{initial} ∪ every chosen policy` on the final pool, which fixes `finalPolicyScore` and the
experiment's `selectedPolicyId`) is logged the same way with `iteration −1`, and its verdicts are
also `DreamLoopResult.finalSelection` / `ExperimentArmResult.finalSelection`. The log is written on
the local and the LLM path alike; a fixed-policy run's file holds only its `−1` step line.
`readDreamsLog(path)` parses it (a missing file is an empty log). Before the log existed a run kept
only a candidate count, and the twelve LLM candidates of run 2 were lost with the reason each lost.

## The shared decision interface

Both online and replay drive the **same** policy interpreter through one `ObservationView`:
`maxParallelism` (`W`), `round`, `observed()`, `legalActions()`, `legalRoots()`, `bestScore()`, and
`revealedNonRootCount()`. Online builds a `LiveObservation` over the growing tree; replay builds a
`ReplayObservation` over the recorded tree plus a revealed-id set. `interpretPolicy(policy, view)`
consumes only the view (the default rules are deterministic and use no RNG), so the identical policy
JSON drives both phases.

A batch is legal when its cells are distinct, all currently legal, at most `W`, and never contain
both a node and its child. `assertLegalBatch(view, cells)` enforces this; the interpreter pre-filters
so it never emits an illegal batch, and the drivers assert it defensively. The exclusion has a fixed
price: a fresh tree cannot fill its first rounds (run 2's trees reveal 1, 1, ≤2, 3, 3, 3 per
round), which at `W = 3`, `k1 = 6` is 5 of 18 possible probes (28% of the budget) and at `k1 = 12`
the same 5 of 36 (14%). Keep `k1` at the default 12 rather than shrinking it to the size of run 2.

## The exploration policy is data, never code

The soundness invariant of the whole subsystem: **a policy is a flat JSON object of numbers and
named-rule string literals, never code.** A fixed TypeScript interpreter is the only thing that acts
on it. `parseExplorationPolicy` rejects any unknown key, any out-of-range number, any non-integer
where an integer is required, and any named rule outside `SELECTION_RULES` / `RECOVERY_POLICIES` /
`STOP_RULES`, throwing `PolicyValidationError`. A stringified function or code payload is rejected as
an unknown-key / type error. There is no `eval`, no `Function`, no child process, and no dynamic
import of policy content anywhere.

The ten fields are `selectionRule`, `recoveryPolicy`, `stopRule`, `branchWidth`, `refineDepth`,
`batchSize`, `beta` (patience / round cap), `promisingThreshold`, `targetScore` and
`explorationBias`. Replay reads seven of them. `REPLAY_DEAD_FIELDS = [branchWidth, refineDepth,
recoveryPolicy]` (`policy.ts`) are never read by the replay simulator: `branchWidth` and
`refineDepth` shape every ONLINE proposal (`projectProposeParams` feeds them to the proposer and
its prompt) and `recoveryPolicy` is read nowhere. A candidate that differs from the current policy
only in them replays identically, so dreaming can never measure, and therefore never adopts, a
step-size change; the local dreamer does not mutate them, the LLM dreamer's prompt says so, and a
candidate that changes only them is reported `unmeasurable`. Dreaming optimizes the other seven — by
local search over the parameter space by default, or optionally an LLM that emits a **constrained
policy JSON that is parsed through the same validator**, so a bad LLM policy can never regress the
deployed one and never executes.

## Determinism

All randomness flows through one injected `SeededRng` (splitmix64 over a masked 64-bit BigInt state,
forkable by hashing `seed + label`, each fork independent of the parent's draws) and one injected
`DreamClock = () => number`. The tree, replay and objective core never call `Date.now`, `Math.random`
or `randomUUID`. Replay and the lever scan use no RNG at all (deterministic ranking). Node ids are
`<treeId>-n<seq>`; the clock-derived ids are the `treeId` at the outer boundary, the priming tree
ids, and the run id (`dreamRunId`, minted from `clock()` when the run STARTS), which is also the key
of the `dreams/` and `rejections/` logs. Two runs with the same seed into two fresh stores produce
identical tree ids, identical final policy ids and scores, and byte-identical tree files; the
dreams log and the round records touch neither rng nor tree, so a run is byte-identical with and
without them.

## Persistence (JSONL + blobs)

The store mirrors the RAVO archive layout. The directory is `PRIME_AGENT_DREAM_DIR` if set (tilde
expanded), else `<agent-dir>/dream`. Trees are `<dreamDir>/trees/<treeId>.jsonl`; full artifacts are
`<dreamDir>/trees/<treeId>/blobs/<seq>.json`, so node lines stay scalar-only. Beside `trees/`
(never inside it, so `listTrees` does not see them) sit the per-run logs
`<dreamDir>/dreams/<runId>.jsonl` (every dreaming step, both paths) and
`<dreamDir>/rejections/<runId>.jsonl` (rejected child results, LLM path only); an experiment arm's
store is `<dreamDir>/experiments/<experimentId>/<arm>/` with the same three subdirectories.

- **Header** (line 0): `{type:"tree",version,treeId,taskId,n?,w,seed,policyId,iteration,createdTs}`.
- **Node line**:
  `{type:"node",id,parentId,branch,seq,round,score,valid,failClass?,origin,artifactRef,tokens,ts}`
  with `id = <treeId>-n<seq>` and `ts` from the injected clock. `origin` is provenance: `root` for
  the seeded root, `local` for the zero-token mutator, `llm` for a candidate a child agent
  generated. On the LLM-proposer path a `local` non-root node is a FALLBACK (the child's output was
  rejected and the mutator stood in), and its `tokens` are what that rejected child spent. A line
  written before provenance existed has no `origin` and reads as `root` / `local` (`nodeOrigin`).
- **Reveal line** (one per online round): `{type:"reveal",round,ids:[...]}` — informational for
  `show`; replay derives reveals from `seq` + `parent` and does not read it.

The header is written with a create/truncate so a same-seed re-run is idempotent; node and reveal
lines are appended. Scores persist as `valid:boolean` plus a **finite** `score:number` (0 when
invalid) — never `Infinity`/`NaN`, which `JSON.stringify` would turn into `null`. Eligibility and the
objective gate on `valid`.

## The tasks

`DREAM_TASK_IDS` (`core/dream/tasks/index.ts`) is the one source of truth for the task ids:
`circle-packing`, `sum-difference`, `python-speedup`, `autocorrelation`. The CLI usage, the `/dream`
usage and the `dream.run` / `dream.experiment` host error strings are built from it; the kernel
skill's `_TASKS` mirrors it (Python cannot import the TypeScript array) and the host re-validates
every request. The three numeric tasks are pure, deterministic, and need no GPU or external API;
the code task is a real bounded subprocess.

- **Circle packing** (`circle-packing`, `n ∈ {26, 32}`): choose centers `(x_i, y_i) ∈ [0,1]²` and
  radii `r_i ≥ 0` with each circle inside the unit square and no overlap, maximizing `Σ r_i`. The
  root is a jittered grid; `propose` perturbs centers by a seeded gaussian scaled by the policy's step
  and then assigns a **feasible** radius per circle
  (`r_i = ½·min(boundaryDist_i, min_{j≠i} dist(i,j))`), which is always valid though not maximal.
  `evaluate` independently rechecks validity and scores `Σ r_i`, or `valid:false / score:0`.
- **Sum-difference** (`sum-difference`): a finite integer set `A`, maximizing
  `Γ(A) = log(|A+A|/|A|) / log(|A−A|/|A|)`. Edits are seeded add/remove/replace within a bounded
  window keeping `|A| ≥ 2`. Degenerate sets (a singleton, an arithmetic progression) score
  `valid:false / score:0`, never `NaN`/`Infinity`.
- **Autocorrelation** (`autocorrelation`, `n ∈ {32, 64, 128}`, default 64; the paper's Appendix A
  first autocorrelation inequality): a step function of `n` non-negative bin weights on
  `[−1/4, 1/4]`, normalized to integral 1, minimizing the peak of its autoconvolution; the score is
  `1 / peak` (the uniform root scores 0.5, every score is below 0.782), computed EXACTLY as
  `h · max_k Σ_{i+j=k} w_i w_j` with no quadrature. An all-zero vector is `valid:false / score:0`.
  The proposer prompt context names the exact size (`exactly 64 weights`); this is the task run 2
  measured.
- **Python speedup** (`python-speedup`): the artifact is a Python 3 program (sum over pairs of
  `|a_i - a_j|` from stdin); the root is a correct but deliberately slow reference. `evaluate` runs
  the candidate under `python3 -I -B` with a minimal environment and a strict timeout, checks HIDDEN
  tests first (any failure scores 0), then scores `baselineTime / candidateTime`, capped. The
  proposer sees only the public contract (`PYTHON_SPEEDUP_PROMPT_CONTEXT`). Because `evaluate` is
  wall-clock timed, this task's scores are not byte-deterministic; everything else (tree shapes,
  ids, replay over recorded scores) is.

## The CLI

```
prime-agent dream [rollout|replay|improve|loop|experiment|status|show] [options]
```

The subcommand is the first positional; absent, it defaults to `loop`. Aliases: `propose` → rollout,
`simulate` → replay, `compare` → experiment, `inspect` → show. Common flags: `--task`, `--n` (the
task's size parameter, threaded to every task that takes one: any integer `>= 2` for circle-packing,
a bin count in {32, 64, 128} for autocorrelation; the parser turns the task's `RangeError` into a
usage error, and the size the run was built with — `--n` or the task default — is recorded on the
result, every tree header and the header line), `--seed` (default 1), `--workers` (`W`, default 4), `--k1` (default 12), `--k2` (default 24),
`--dreams` (`M`, default 16), `--beta1` / `--beta2` / `--beta3` (the replay objective's weights,
finite and non-negative, `beta3` in `[0, 1]`; defaults 0.05 / 0.10 / 0.25), `--iterations`
(default 3), `--priming <none|diverse>` (default `none`; `diverse` rolls out `PRIMING_DIVERSE` at
round 1, for `loop` and `experiment`), `--tree <id|latest>`, `--dir <path>`, `--json`,
`--llm-proposer`, `--llm-dreamer`. `experiment` takes `--rounds` (default 4), `--arms`
(default `dream,fixed`), `--seeds a,b,c` (one experiment per seed, sequentially, each result path
printed; distinct non-negative integers, no count cap on the standalone CLI) and `--overwrite`
instead of `--iterations`. `replay`, `improve`, `loop` and `experiment` all score with the three
betas, and `replay` and `improve` measure the cost term against `--k1` (the budget is the tree's
`W` times `k1`). The defaults `k1 12` with `DEFAULT_POLICY.beta 6` keep the stop rule live (see the
`k1 ≤ beta` note under [Stores and result](#stores-and-result)).

| subcommand | what it does | span opened |
|---|---|---|
| `loop` (default) | rollout → dream → redeploy for `--iterations`; prints per-round best score and probes (paper Fig. 6), initial vs final policy, best node score, tree ids, tokens (0 locally); header line carries `beta1 beta2 beta3` | `dream.run` wrapping `dream.explore` / `dream.dream` / `dream.redeploy` |
| `experiment` (`compare`) | runs every `--arms` arm from the same initial policy, seed, clock and budget for `--rounds` rollouts each, the `fixed` arm never dreaming; prints a per-arm round table with provenance and dreaming lines and the headline card, writes `experiments/<id>/result.json` (see [Experiments](#experiments-the-fixed-exploration-control)) | `dream.experiment` wrapping one `dream.experiment_arm` → `dream.run` per arm |
| `rollout` (`propose`) | one online exploration with the default policy; persists the tree; prints tree id, rounds, revealed count, best score, best node id | `dream.explore` |
| `replay` (`simulate`) | loads a recorded tree (`--tree`), re-walks it with the default policy, prints `revealed N`, `rounds`, `best`, `out-of-support`, `in-support`, `probes to best` and `V` with the three betas and the budget; zero execution | `dream.replay` (root) |
| `improve` | freezes the task's tree pool, runs the local dreaming search, prints chosen vs current policy and candidate ids | `dream.dream` |
| `status` | read-only store summary: tree count, per-task counts, best node score, last policy id, store dir | none |
| `show` (`inspect`) | prints one tree's header (with `agent-generated a/n`), per-round reveals, and each node with its `origin` | none |

`--json` prints the result object as JSON (`dream replay --json` dumps the whole `ReplayResult`,
`bestSoFar` included). Exit codes: `0` success; `1` a usage error (bad or unknown flag, or `--n`
outside the task's accepted sizes); `2` an LLM flag on the standalone CLI, or a read subcommand
(`status`/`replay`/`show`/`improve`) run against an empty store or pool.

### The `dream experiment` text output

Counts only, never a path or a clock (the two-clock byte-identity test in
`test/dream-command.test.ts` requires every non-id line to be clock-free). Per arm:

```
    round | best | cum best | probes | agent | fallback | cum probes | policy
        1 | <best> | <cum> |     <p> |   <a> |      <f> |        <cp> | <policyId>
        2 | <best> | <cum> |     <p> |   <a> |      <f> |        <cp> | <policyId>  dreamed <current V> -> <chosen V> improved <bool>
      dreaming: candidates <n>  eligible <e>  winner <policyId>|tie (current kept)  lever gap <+/-gap> (<P> policies, <E> eligible)  dreamer <llm|local|mixed>
    final policy <id>  changes <c>  selected policy <id>  own-pool score <initial> -> <final>  final best <b>  probes <n>  handler calls <n>  tokens <n>
    provenance: <n> probes = <a> agent-generated + <l> local (<f> fallbacks); local proposer, 0 LLM proposals
    dreaming: <k> phases  improved <i>/<k>  policy changes <c>
```

Scores print with six decimals, multipliers with two. The `dreaming:` line under a row appears only
when the record carries `candidateVerdicts`. On the LLM path the provenance line ends
`<P> LLM proposals = <A> accepted + <R> rejected (parse n, shape n, ...)`. An arm that dreamed but
never changed its policy ends `-> INERT (the arm ran its initial policy throughout)`. After the
rollout-granular headline the card prints the exact one when the result carries it:

```
  exact headline (probe-granular): target reached by fixed at probe <n>|target not reached by the reference
    dream: target at probe <n> -> <x.xx>x fewer calls|not comparable|target not reached
```

### Why the LLM flags are rejected

The zero-token guarantee is structural. The barrel the CLI imports (`core/dream/index.ts`) does not
re-export `llm.ts` (nor `dreams.ts`, which callers import directly); `dream-command.ts` imports only
`core/dream/index.js`; and the `--llm-*` rejection is pure argv inspection. `llm.ts` (which reaches
the child-agent call path via `createRunAgentChildCall`) is only reachable from in-session callers
that hold a `RunAgentHandler`. The standalone CLI has none, so `--llm-proposer` / `--llm-dreamer`
return exit 2 with:

```
LLM proposer/dreamer require an in-session agent handler and are unavailable from the standalone CLI; the default local proposer runs at zero tokens.
```

No token is spent and no socket is opened.

### The LLM proposer's output contract and rejection provenance

A real run showed the failure the local fallback hides: on one model 81 of 83 `dream.llm_propose`
spans fell back because the child wrapped its JSON in an explanation or code fences, returned the
wrong `weights` length, or (with thinking on) ran to a 32k-token output cap; every fallback was
recorded as a normal node with the child's tokens and no mark, so "agent calls" overstated the
agent's work and the rejection cause was gone. `createLlmProposer` (`llm.ts`) now works like this:

- **Extraction.** The child's output is searched for its JSON object with `extractJsonValue`
  (`ravo/runtime-adapter.ts`): the whole output is tried first, then every `{` is a candidate start,
  the balanced close is found by a string-aware scan, and the LARGEST candidate that parses wins, so
  fences, prose before or after the object, and a small fragment quoted in an explanation are all
  tolerated. The dreamer (`array`) and guidance writer (`object`) opt in the same way through
  `StructuredChildSpec.extractJson`; RAVO's own children keep the strict whole-output parse.
- **Prompt.** The proposer prompt is header, optional guidance, the current candidate, the
  generation hints, the task contract from `taskPromptContext(task, resolveTaskN(spec))` — which
  names the exact size (`"n": 64 and exactly 64 weights`) and carries a parseable shape example —
  then an output contract (one JSON object in the candidate's shape, no prose, no fences, no thinking
  out loud, keep it as short as the object) and, as the LAST line, `PROPOSER_JSON_ONLY`.
- **Classification.** Every child result is accepted or rejected with one `ProposalRejectReason`:
  `parse` (no JSON object), `shape` (the task's `deserialize` threw a `TypeError`: wrong keys,
  wrong `weights` length, non-numeric entries), `invalid-candidate` (any other refusal), `error`,
  `turn-limit`, `budget`, `aborted`, and `length` when the child's final message stopped at its
  output cap (a rejection of that output is `length` whatever the parser said). `error`, `parse`,
  `shape` and `invalid-candidate` get one retry (`PROPOSER_RETRIES`); `length` does not, so a
  runaway is never paid for twice. When the last result is rejected the local mutator stands in,
  the node is `origin: "local"`, and the child's tokens stay on it; an accepted result is
  `origin: "llm"`; an abort throws `DreamAbortError`.
- **Records.** The `dream.llm_propose` span carries `dream.llm_attempts`, `dream.llm_output_tokens`,
  `dream.origin` and, on a rejection, `dream.llm_reject_reason`, `dream.llm_status` and a
  240-character `dream.llm_reject_excerpt`. Every rejection is one line of
  `<dreamDir>/rejections/<runKey>.jsonl` (`rejections.ts`; `runKey` is the run id — see
  [Run ids](#run-ids-and-log-keys) — and `<experimentId>-shared` for an experiment's shared round 1,
  written under the first arm's store): `{type:"rejection",ts,role?,iteration,round,attempt,
  reason,status,fellBack,tokens,outputTokens,stopReason?,error?,excerpt}`. `role` is `proposer`
  (absent on lines predating the field) or `dreamer`. The per-rollout `ProposalTally`
  (`llmProposals`, `llmAccepted`, `llmRejected` by reason, `localFallbacks`) is copied onto each
  `DreamRoundRecord.proposals` next to `agentGeneratedCalls`, and the experiment rows and totals
  carry the same fields, so a fallback is never counted as the agent's work.

### The LLM dreamer

`proposePoliciesWithAgent(runAgent, current, m, options)` (`llm.ts`) asks one child for up to `m`
revised policies inside a `dream.llm_dream` span. Its prompt (`buildDreamPrompt` over a
`DreamChildInput` built rng-free by `buildDreamerInput`) states the count contract, the policy
schema with its bounds, how `V` is computed and how the winner is selected (the quality guard, the
strict-improvement rule, current wins ties), the replay mechanics including that an out-of-support
selection is charged, the per-field replay semantics (which fields each rule reads, the `batchSize`
cap at `W`, and that the replay-dead fields cannot win), the current policy with its per-tree replay
on the frozen pool (`DreamerPoolTreeDigest`: `N`, `rounds`, `outOfSupportCells`, `bestScore` and
every objective term, scalars only) and the verdicts of every earlier step (`DreamHistoryEntry`,
from `historyOf`), and ends on the JSON-array-only instruction.

The child's array is parsed entry by entry through the strict `parseExplorationPolicy`
(`parseCandidateArray`: `kept`, `dropped` with the parser's reason, `returned`). The first `m`
DISTINCT entries that differ from the current policy count toward `m`; an identical or duplicate
entry rides along so the selection records it as such; anything past the budget is `truncated`.
A shortfall is topped up from the local `proposePolicies` on the labelled fork
`dream-fallback:<iteration>`, so the step's `dreamer` is `llm`, `mixed` or `local`, and
`llmFallback` is true when the child contributed nothing. A retryable rejection gets one more call;
every rejected result is a `role: "dreamer"` line (`round 0`) in the rejections log and is put on
the span (`dream.llm_reject_reason`, `dream.llm_status`, `dream.llm_reject_excerpt`); an all-dropped
array is a `shape` rejection; an abort throws `DreamAbortError`. The candidates then go through the
same `runDreaming` selection as local ones, so a bad policy is never deployed.

### Child model, thinking and output cap

A child's output and thinking are now bounded from the request, which is what run 2's `length`
rejections asked for:

- `RunAgentRequest.thinkingLevel` (`run-agent.ts`) is validated in `agent-session.ts` against
  `getSupportedThinkingLevels(model)` (`Requested thinking level "X" is not supported by model
  "provider/id"; supported levels: ...`) and applied at child construction through the existing
  `_createRlmSubagentRuntimeOptions({ thinkingLevel })` seam — never via `setThinkingLevel`, which
  would persist it as the user's settings default. Absent, the child inherits the parent's level
  clamped to its model.
- `RunAgentOptions.maxOutputTokens` (a positive integer, validated like `maxTurns` and
  `tokenBudget`) is applied inside `runAgentSession` by wrapping the ephemeral child session's
  `agent.streamFn` with `capStreamFn`, so every model call carries
  `maxTokens = min(existing, cap + THINKING_ALLOWANCE[reasoning])`. The allowance (`minimal` 1024,
  `low` 2048, `medium` 8192, `high`/`xhigh`/`max` 16384, 0 when thinking is off) mirrors
  `simple-options.ts`, so the cap means the VISIBLE answer even on models whose `max_tokens`
  covers thinking too. The wrap is never restored: a caller passing the cap owns a single-use child.
- `ChildRuntimeScope` (`ravo/runtime-adapter.ts`) gains `thinkingLevel` and `maxOutputTokens`;
  `structuredChildRequest` spreads the first onto the request and `structuredChildRunOptions` the
  second onto the run options, both omitted when unset.
- `DreamRunService` fixes the per-role defaults (`DREAM_CHILD_DEFAULTS`): thinking `off` for the
  proposer, dreamer and guidance writer, caps of 4096 (proposer; 8192 for `python-speedup`), 4096
  (dreamer) and 2048 (guidance). On the measured run the thinking-off proposer answered in about
  270 output tokens with 93% acceptance while thinking-on gave 2 of 83. `dreamChildScope` puts the
  thinking level and the PROPOSER cap (or an explicit request cap) on the shared scope, which is
  what the experiment records as the arm mode; `roleCappedRunAgent` gives dreamer and guidance
  children their own default caps by the prompt's first line (`dreamChildRole` over the exported
  `*_PROMPT_HEADER`s) and is the identity when the request set an explicit cap.
- Surfaces: `/dream --model provider/id --thinking <level> --max-output-tokens N`, the `dream.run`
  / `dream.experiment` host payload keys `model`, `thinking` (case-insensitive), `max_output_tokens`,
  and the skill kwargs `model`, `thinking`, `max_output_tokens`. `ExperimentArmMode` records
  `model`, `thinking` and `maxOutputTokens`. Spans: `rlm.run_agent` gains `rlm.requested_thinking`
  and `rlm.max_output_tokens` at start and `rlm.thinking` (the effective level) once the child is
  built; `dream.run` on the in-session path gains `dream.child_model`, `dream.child_thinking` and
  `dream.child_max_output_tokens` when the scope carries them.

## Experiments: the fixed-exploration control

The mechanism above is the paper's Figures 1–2. Its evidence (Figures 3–6) is a comparison against
one control, **Recursive Fixed Exploration**: the identical discovery agent, evaluator,
initialization and per-round budget, with an exploration policy that never changes. `dream
experiment` runs that comparison; `evals/dream/plot_experiment.py` draws it.

### Vocabulary

- A **round** is one rollout of the discovery agent. `--rounds N` means `N` rollouts per arm; the
  loop runs `iterations = N - 1`, and loop iteration `i` is round `i + 1`.
- The **budget per round** is `(W, k1, k2, dreams)`; every arm shares it verbatim.
- **Probes** = evaluated attempts = `ExploreResult.revealedCount` = `tree.size - 1`. This is
  discovery compute, the paper's "agent calls", on every path (local and LLM alike), and the only
  input to the multipliers. Round 1 also counts the priming rollouts' probes when the pool was primed
  (`primingProbes`, included in `probes`).
- **Agent-generated calls** = `ExploreResult.agentGeneratedCount` = the probes whose candidate a
  child agent actually produced (nodes with `origin: "llm"`). On the LLM path
  `probes - agentGeneratedCalls` are **local fallbacks**: attempts whose child output was rejected
  and replaced by the local mutator. The per-round rows carry `agentGeneratedCalls`,
  `cumulativeAgentGeneratedCalls` and `localFallbacks` beside `probes`, so a plot can label which
  series is on its compute axis; the headline multipliers stay on probes. Zero on the local path.
- **Proposals**: what happened to every child proposer result, per round — `llmProposals`
  (results examined, retries included; `= llmAccepted + sum(llmRejected)`), `llmAccepted` (each one
  is an `origin: "llm"` node) and `llmRejected` by reason (`parse`, `shape`, `invalid-candidate`,
  `error`, `length`, `aborted`, `turn-limit`, `budget`; every reason present, 0 when unseen). All
  zero on the local path.
- **Handler calls** per role (`proposer`, `dreamer`, `guidance`; retries included) and child
  **tokens** are cost. They are recorded and plotted as cost and never mixed into the compute axis.
  Zero on the local path.
- **Exact curve**: every round row carries `probesToRoundBest` (the 1-based probe within the
  round's charged probes at which `roundBest` was first reached; 0 for the root) and `improvements`
  (`[{probe, score}]`), the rollout's best-so-far curve; on a primed round 1 the probe index runs over
  the initial rollout's probes and then each priming tree's (`mergedRoundCurve`, with the best valid
  root of the round at probe 0), so the curve's last point is always `roundBest`.
- **`stoppedEarly`** per arm: rollouts (priming excluded) whose stop rule fired before `k1`. Zero
  means no rollout ever stopped before the cap, which `k1 ≤ beta` forces; a reader should check it
  before crediting or blaming the stop rule.

### Arms

| arm | dreams | proposer prompt | needs |
|---|---|---|---|
| `dream` | yes | plain | nothing (local) |
| `fixed` | never (`fixedPolicy: true`) | plain | nothing (local) |
| `dream-guided` | yes | prefixed with prior-trajectory insights | in-session LLM proposer |
| `fixed-guided` | never | prefixed with prior-trajectory insights | in-session LLM proposer |

Every arm starts from the same hand-written initial policy (`initialPolicy`, default
`DEFAULT_POLICY`) and the same seed. Because `rng.fork` is label-derived, the loop never draws
from the root rng, and every attempt fork is labelled by round, parent seq and child slot
(`attemptRngLabel`) rather than by a node id, each round's rollout rng stream is the same in every
arm and in every run of that seed; with the same clock the tree ids are too. Round 1 is therefore
identical across arms by construction: on the local path each arm rolls it out itself and the tree
files are byte-identical; on the LLM path it is rolled out once and copied into every arm's store
(`sharedInitialRollout: true`), so no tokens are spent twice on it.

The `fixed` arm (`fixedPolicy` on `runDreamLoop` / `runDreamLoopWithAgent`) skips `freezePool` and
dreaming on every iteration and redeploys the initial policy inside the usual `dream.redeploy`
span, so its pool grows exactly like the dreaming arm's and only the policy differs. Its
`finalPolicyId` equals its `initialPolicyId`, `improved` is false and every `dreaming` record is
null.

### Pooling policy

A dreaming arm's replay pool is every tree in ITS store: it grows across rounds within the arm and
never across arms or seeds. The control's trees are the control's compute; sharing them would hand
the dream arm replay support it did not pay for and break the paired headline (and, on run 2, the
pooled seven-tree set opened no gap anyway, since the same policy grew every tree). A seed is an
independent replicate, which is what the plotter's noise floor assumes, so there is no cross-store
pooling API to switch on; "a bigger pool" means more `--rounds` in one arm, not more arms or seeds.
The tree ids DO collide across arms of one seed — round `r` has the same id in every arm — and that
is intended: the shared round 1 depends on it, and the arms' stores are separate directories.

### Pool priming

`primingPolicies` (`DreamLoopOptions`, `DreamLoopWithAgentOptions`, `ExperimentSpec`; default none,
byte-identical to today) roll out once each at iteration 0 on their own labelled forks
(`prime:<i>`, tree id `<task>-s<seed>-i0p<i>-<clock>`) so the frozen pool holds branches the
initial policy would never open. Their trees join the pool, their probes, agent calls and tokens are
charged to round 1 (`primingTreeIds`, `primingProbes`; `roundBest` is the max over the initial and
priming rollouts), and the pool size a dreaming round reports counts them. On the experiment path
they are part of the shared round 1: the LLM runner rolls them out into the first arm's store and
copies every tree into every arm (`runDreamLoopWithAgent` ignores `primingPolicies` when
`initialRollout` is set, because the shared round 1 carries its own), and the local runner rolls
them out per arm identically. `--priming diverse` (CLI and `/dream`), `priming="diverse"` (skill)
and `priming: "diverse"` (host payload) select the fixed set `PRIMING_DIVERSE`: explore-root /
never / `batchSize 8` (breadth at the root) and best-first / never / `batchSize 1` (one greedy
chain), `batchSize` clamped to `W` by the interpreter; `none` is the default and is never recorded.
`dream.run` carries `dream.priming_policies`. Two consequences to know: the explore-root member's
`batchSize 8` is inert (`interpretPolicy` cuts a batch that never holds a node and its parent, and
explore-root only ever grows direct root children, so its batch is always `[root]` and the tree
gains one root child per round), and a priming tree enters the selection's measured pool only when
the incumbent replays it in full support, which a policy that did not grow it rarely does. Priming
therefore widens the record (probes, curves, `roundBest`) and the lever scan's support check, but
it does not by itself give the incumbent a second baseline; on the recorded autocorrelation run
both priming trees were unmeasured for `DEFAULT_POLICY`.

### Run ids and log keys

`dreamRunId(taskId, seed, clock(), runLabel)` is `<task>-s<seed>-r<clock>`, plus `-<label>` when a
`runLabel` is given (characters outside `[A-Za-z0-9._-]` become `_`). It is minted from the clock
when the run STARTS and is the key of the run's `dreams/` and `rejections/` files. The experiment
runner passes `runLabel = <experimentId>/<arm>` (`ExperimentArmLoopOptions`) and
`dreamsLogContext = { experimentId, arm }`, so under the experiment's one frozen clock the arms get
distinct run ids (`...-r<clock>-<experimentId>_dream`, `...-r<clock>-<experimentId>_fixed`) and
every log line names its experiment and arm; before this, both arms of run 2 shared the run id
`autocorrelation-s7-r1789858196752`. A standalone loop keeps the bare form.

### Stores and result

Each arm gets its own store, `<dream dir>/experiments/<experimentId>/<arm>/trees/` (with `dreams/`
and `rejections/` beside it), so an arm's frozen pool holds only its own trees; `listTrees`,
`freezePool` and `dream status` read `<dream dir>/trees` only and never see an experiment. The
experiment id is `<task>-s<seed>-n<rounds>-<clock>`; an existing result is refused unless
`--overwrite`.

The result is `<dream dir>/experiments/<experimentId>/result.json`, schema
`prime-agent.dream.experiment/1`: the spec (task, `n`, seed, rounds, budget, the three betas,
initial policy id and policy, `scoring`), one entry per arm with `storeDir` (relative to the dream
dir), `runId`, `mode` (`proposer`, `dreamer`, and on the LLM path `model`, `thinking`,
`maxOutputTokens`), `initialPolicyId` / `finalPolicyId` (the last deployed policy) /
`selectedPolicyId` (the post-hoc winner), the policy's score on the arm's own final pool, the
per-round rows (`treeId`, `policyId`, `roundBest`, `cumulativeBest`, probes and cumulative probes,
`agentGeneratedCalls` and `cumulativeAgentGeneratedCalls`, `localFallbacks`, `llmProposals`,
`llmAccepted`, `llmRejected` by reason, `decisionRounds`, `poolSize`, per-role handler calls,
tokens, `probesToRoundBest`, `improvements`, round 1's `primingTreeIds` / `primingProbes`, and the
`dreaming` record or null — `currentScore`, `chosenScore`, `improved`, `candidates` (a count),
`candidateVerdicts`, `dreamer`, `leverScan`), `policyChanges`, `stoppedEarly`, `finalSelection`
(the `−1` verdicts), the arm totals (the same provenance counts summed), the `headline` block,
`sharedInitialRollout`, `createdTs` and `notes`. Every field added since the first release is
additive: a result written before it still validates, and a reader must treat its absence as "not
recorded", never as 0. Every number is finite; an undefined value is `null`, never `NaN`. On the
local path the whole file is JSON-equal across repeat runs with the same seed and clock
(`storeDir` is relative), every tree file is byte-identical, and the fixed arm's round-`i` tree id
equals the dream arm's. Two runs of the same seed under different clocks differ only in the
clock-bearing identity — the experiment id, run ids, tree ids and timestamps — and print identical
round tables, policies and headline: the clock never reaches an rng stream or an ordering.
`python-speedup` is the one exception, for score values only: its `evaluate` is wall-clock timed,
and the result's `notes` say so.

`notes` always ends with `objective: normalized (q in pool range, cost in budget fractions)`,
which distinguishes the file from one scored by the raw-scale objective; a file whose `objective`
lacks `beta3` was scored by the two-term form and the plotter refuses to pool it with one that has
it. When the round cap is at most the initial policy's `beta` under `patience` or `fixed-rounds`,
`planExperiment` adds the note (`k1StopRuleNote`) `k1 6 <= initialPolicy.beta 6: patience can
never stop a rollout before the round cap, so every rollout runs exactly k1 rounds and replay
cannot reward saving rounds` — run 2's regime — instead of throwing, and `stoppedEarly` on every
arm then reads 0.

The policy score on an arm's own pool is an in-arm replay estimate: it is reported, and it is never
compared across arms.

### Headline definitions

These are `computeHeadline` in `core/dream/experiment.ts`. With `C_a(r)` the cumulative probes of
arm `a` after round `r`, `P_a(r)` its cumulative best, and `fixed` the reference:

1. target `T = P_fixed(N)`, the fixed arm's final best; `equalBudget B = min_a C_a(N)`, the
   smallest arm total, so every arm has a round inside it unless its first round alone exceeds it.
2. `probesToTarget(a) = min { C_a(r) : P_a(r) >= T - 1e-9 }`, the compute at the FIRST round that
   reaches `T`; `null` when no round does ("not reached"). For `fixed` this is the compute at the
   first round it reached its own final best, not necessarily its total.
3. `bestAtBudget(a) = P_a(r*)`, `r* = max { r : C_a(r) <= B }`; `null` when `C_a(1) > B` ("not
   comparable").
4. **X x fewer calls** `callsMultiplier(a) = probesToTarget(fixed) / probesToTarget(a)`; `null`
   when either term is `null` or the denominator is 0. Exactly 1 for the reference. Below 1 means
   the arm needed MORE calls and is reported as is.
5. **Y x higher score** `scoreMultiplier(a) = bestAtBudget(a) / bestAtBudget(fixed)`; `null` when
   either term is `null` or the reference best is 0, in which case the absolute delta at budget
   (which the plotter prints) is the number to quote. Exactly 1 for the reference.
6. `deltaBest(a) = P_a(N) - T`, the final-best delta against the control; 0 for the reference.
7. **The exact headline.** `probesToTargetExact(a)` (`exactProbesToTarget`) is `C_a(r − 1)` before
   the first rollout whose `improvements` curve reaches `T − 1e-9`, plus the probe index of that
   improvement: the compute at the FIRST PROBE that reached `T`, not the end of the round that
   contains it. `null` when never reached or when any round of the arm lacks `improvements` (an
   older loop), which is "not recorded", not "not reached". `callsMultiplierExact(a) =
   probesToTargetExact(fixed) / probesToTargetExact(a)`. The rollout-granular fields stay; the CLI
   and the plotter print both.
8. Fig. 6b adaptivity is not a multiplier: it is the per-round probes series plus `policyChanges`,
   the number of rounds whose `policyId` differs from the previous round's. The fixed arm's series
   is flat in expectation.
9. Guidance ablation: for each (unguided, guided) pair that ran, `P_guided(N) - P_unguided(N)` and
   both `probesToTarget` values (the plotter derives it from the rounds). The paper's claim is
   confirmed only when the delta is negative; the sign is printed either way.

Nothing is clamped. Ratios are stored as full doubles and shown with 2 decimals plus the raw
operands (`1.11x fewer calls (27 vs 30)`). With `N = 1` every arm ties at 1.00, and the plotter
labels an `N < 3` experiment as too short to show a curve. Without a `fixed` arm the headline is
`null` and a note says so.

### Semantic-guidance ablation

The paper's ablation injects "directional insights" from prior trajectories into the discovery
agent's prompt and finds it underperforms unguided exploration. The `dream-guided` and
`fixed-guided` arms reproduce that on the LLM-proposer path (`semanticGuidance` on
`runDreamLoopWithAgent`, which requires `useLlmProposer`). On every iteration `>= 1` the loop
freezes the pool, builds a bounded, deterministic digest of it (`buildGuidanceInput`: per tree the
policy id, best score, attempts, rounds, fail classes and the top-k recorded artifacts, truncated;
recorded artifacts and scalar scores only, never hidden tests), and asks one guidance-writer child
call (span `dream.llm_guidance`) for 3–8 insights, which `buildProposePrompt` inserts after the
proposer header only when non-empty. Iteration 0 has no pool, so its prompt is byte-identical
with and without guidance. A failed guidance call falls back to empty guidance
(`dream.llm_fallback: true`); an aborted one aborts the run. Its calls and tokens are recorded
under the `guidance` role.

### Running an experiment

Standalone (local arms only, zero tokens):

```
prime-agent dream experiment --task circle-packing --rounds 4 --seeds 1,2,3 --dir /tmp/dream-evidence
prime-agent dream experiment --task autocorrelation --rounds 4 --seed 7 --priming diverse --json
```

`--seeds` runs one experiment per seed and prints each result path. A guided arm on the standalone
CLI exits 2 with `ExperimentArmUnavailableError` and writes nothing; `--llm-proposer` /
`--llm-dreamer` stay rejected as before.

In-session, `/dream experiment --task <id> --rounds N --arms dream,fixed,dream-guided
--llm-proposer` and the kernel's `await dream.experiment(task, rounds, arms=..., llm_proposer=True)`
run the same experiment through `DreamRunService.startExperiment`, with progress in the Agents
View. Every non-local arm spends tokens.

**Several seeds in one run.** `/dream experiment --seeds 7,8,9` (exclusive with `--seed`), the
skill's `seeds=[7, 8, 9]` and the host payload's `seeds` run the seeds sequentially in the single
run slot under ONE service run id, one `dream_run_update` stream and one frozen clock: each seed is
an independent experiment with its own id `<task>-s<seed>-n<rounds>-<startedAt>`, stores and
`result.json`. At most `DREAM_MAX_SEEDS = 16` distinct non-negative integers (`validateDreamSeeds`
throws `RangeError`; the host payload errors read `dream.experiment seeds must be a non-empty array
of distinct non-negative integers`, `... must list at most 16 seeds (got N)` and `dream.experiment
takes either seed or seeds, not both`; the skill raises `TypeError`), because every LLM seed is a
full experiment's spend. `DreamRunStatus` gains the additive optional `seed`, `seedIndex`,
`seedCount`, `resultPaths` (every completed seed's result, in order; `resultPath` stays the last
completed one) and, on the LLM path, `tokens` (the last completed seed's total over all arms, so the
spend is visible mid-run); the existing `kind`, `experimentId`, `arm`, `armIndex`, `armCount`,
`round`, `rounds`, `cumulativeProbes` and `resultPath` are unchanged, so there is still no protocol
bump. A cancel keeps every completed seed's `result.json` and ends `cancelled` with the completed
`resultPaths`. The completion row lists every result path (`Dream-RSI experiment <runId> completed
(results p1, p2, p3)`), and the Agents View line reads `dream experiment seed 2/3 dream 1/2 r3/4
dreaming best 0.5884 tokens 163412` while running and `dream experiment completed (3 result files)`
when done.

### Plots

```
python3 evals/dream/plot_experiment.py <dream dir>/experiments/<id>/result.json [--out <dir>] [--check]
```

Several result files are seeds of one experiment. The script writes `round_best.png` (Fig. 6a),
`compute.png` (Figs. 3b/5, with `T` and `B` as guide lines), `attempts.png` (Fig. 6b, policy changes
marked, and the `policy never changed ... dreaming inert` stamp), `proposals.png` (the proposer's
accepted / rejected / fallback counts), `dreaming.png` (the per-candidate audit read from
`candidateVerdicts`, with a fallback for files that predate it), `headline.png` (the multipliers or
the literal words `not reached` / `not comparable`, the exact headline beside the rollout-granular
one, the fixed arm's noise floor across seeds, the paired per-seed deltas and the verdict —
`single seed: no verdict`, `exceeds noise floor` or `within noise floor`, forced to `within noise
floor (dreaming inert)` when the dream arm's policy never changed in any seed) and a
self-contained `report.html`. `--check` prints the reduced tables without matplotlib and verifies
the file's headline arithmetic against the rounds (`headline recomputed ... agrees with the file:
yes`). Re-plotting run 2 reads `single seed: no verdict (dreaming inert: the arms ran the same
policy)`. `evals/dream/README.md` has the details, including the interpreter fallback when the
system `python3` lacks matplotlib.

## In-session vs standalone spans

The sync CLI drivers open `dream.run` and, inside it, `dream.explore` / `dream.dream` /
`dream.redeploy` as direct children — `withSpan` restores the parent context when each sync child
returns, so no child outlives its parent and there are no detached roots. The async in-session
driver (`runDreamLoopWithAgent`, in `llm.ts`) runs past the user turn, so it mints ONE detached
root, `dream.run` (`startSpan` after leaving the ambient context, carrying the launching turn's
`trigger.trace_id`, ended in `try/finally` on success, abort and error), and then runs the whole
loop body inside that span's context: `dream.explore` / `dream.round` / `dream.attempt` /
`dream.llm_propose`, `dream.llm_guidance`, `dream.llm_dream`, `dream.dream` (with its `dream.replay`
and `dream.candidate` children) and `dream.redeploy` are ordinary `withSpan` children of
`dream.run` on both paths. Likewise an in-session experiment mints `dream.experiment` as the
detached root, its `dream.experiment_arm` spans are children, and each arm's `dream.run` is its own
detached root whose `trigger.trace_id` is the experiment's trace (`experiment.ts`,
`runExperimentWithRunner`). Run 2's trace confirms it: every `dream.dream` and `dream.llm_dream`
span has `dream.run` as its parent. `DreamRunService` opens no span of its own.

## Observability rows

These spans are opened under `packages/coding-agent/src/core/dream/*`. All attribute values are
scalars (`string | number | boolean | undefined`) and every span ends on throw and abort. The
authoritative rows are in `docs/observability.md`; this table is the per-file map:

| span | opener | parent | attrs |
|---|---|---|---|
| `dream.experiment` | `experiment.ts` `runExperiment` (sync, in-turn root) / `runExperimentWithRunner` (detached root carrying the launching turn's `trigger.trace_id`) | root | `dream.experiment_id`, `dream.task`, `dream.seed`, `dream.rounds`, `dream.arms`, `dream.mode`, `dream.stopped` (`aborted`) |
| `dream.experiment_arm` | `experiment.ts` | `dream.experiment` | `dream.experiment_id`, `dream.arm`, `dream.fixed_policy`, `dream.guided`, `dream.run_id` |
| `dream.run` | `loop.ts` `runDreamLoop` (sync) / `llm.ts` `runDreamLoopWithAgent` (async) | root of a CLI loop; under an experiment a child of `dream.experiment_arm` (sync) or a detached root whose `trigger.trace_id` is the experiment's trace (async) | `dream.task`, `dream.seed`, `dream.workers`, `dream.k1`, `dream.k2`, `dream.dreams`, `dream.iterations`, `dream.mode`, `dream.fixed_policy`, `dream.priming_policies`, `dream.run_id`; async only: `dream.child_model`, `dream.child_thinking`, `dream.child_max_output_tokens` (when the scope carries them), `dream.stopped` (`aborted`), `trigger.trace_id` |
| `dream.explore` | `rollout.ts` `runOnlineExploration` / `llm.ts` `runOnlineExplorationWithAgent` | `dream.run` (iteration 0), `dream.redeploy` (every later rollout), or `dream.experiment` (the LLM path's shared round 1) | `dream.tree_id`, `dream.policy_id`, `dream.k1`, `dream.workers`, `dream.iteration` |
| `dream.round` | `rollout.ts` / `llm.ts` | `dream.explore` | `dream.round`, `dream.batch_size`, `dream.revealed_count`, `dream.best_score` |
| `dream.attempt` | `rollout.ts` `commitAttempt` | `dream.round` | `dream.node_id`, `dream.parent_id`, `dream.task`, `dream.valid`, `dream.score`, `dream.tokens`, `dream.origin`, `dream.fail_class?` |
| `dream.replay` | `replay.ts` `simulatePolicyWithSpan` (standalone root; bare `simulatePolicy` opens none so it can run in a tight dreaming loop) / `improve.ts` `runDreaming` (one coarse per-step child of `dream.dream`) | root or `dream.dream` | standalone: `dream.policy_id`, `dream.tree_id`, `dream.revealed_n`, `dream.rounds`, `dream.v`, `dream.out_of_support`, `dream.in_support`, `dream.probes_to_best`, `dream.simulations` (1); per step: `dream.policy_id`, `dream.iteration`, `dream.simulations` (the exact replays the selection made: current on every tree, each simulated candidate on the measured trees; the lever scan's are on `dream.dream`), `dream.measured_trees` |
| `dream.dream` | `improve.ts` `runDreaming` | `dream.run` (both paths) | `dream.candidates`, `dream.pool_size`, `dream.iteration`, `dream.chosen_policy_id`, `dream.chosen_score`, `dream.current_score`, `dream.chosen_quality`, `dream.current_quality`, `dream.quality_rejected` (verdicts with that reason; disjoint from `dream.unmeasurable`), `dream.unmeasurable`, `dream.improved`, `dream.dreamer`, `dream.in_support_current`, `dream.measured_trees`, `dream.simulations`, `dream.lever_gap`, `dream.lever_policies`, `dream.lever_simulations` (the last three absent when the lever scan is off) |
| `dream.candidate` | `improve.ts` `runDreaming` (post hoc, one per candidate, zero duration) | `dream.dream` | `dream.iteration`, `dream.candidate_index`, `dream.policy_id`, `dream.origin`, `dream.reason`, `dream.eligible`, `dream.value`, `dream.quality`, `dream.anytime`, `dream.cost`, `dream.rounds_saved`, `dream.in_support_min`, `dream.changed` (comma-joined field names) |
| `dream.redeploy` | `loop.ts` / `llm.ts` | `dream.run` (both paths) | `dream.policy_id`, `dream.k1`, `dream.workers`, `dream.iteration`, `dream.fixed_policy`, `dream.tree_id`; it wraps the rollout's own `dream.explore` |
| `dream.llm_propose` | `llm.ts` `createLlmProposer` (LLM-proposer path only; one per generation attempt, wrapping every child call of that attempt) | `dream.round` | `dream.round`, `dream.tokens`, `dream.llm_output_tokens`, `dream.llm_attempts`, `dream.llm_fallback`, `dream.origin`, `dream.llm_reject_reason?`, `dream.llm_status?`, `dream.llm_reject_excerpt?` |
| `dream.llm_dream` | `llm.ts` `proposePoliciesWithAgent` (LLM-dreamer path only; one per dreaming step) | `dream.run` | `dream.candidates_requested`, `dream.iteration`, `dream.tokens`, `dream.llm_attempts`, `dream.candidates_returned`, `dream.candidates_dropped`, `dream.candidates_kept`, `dream.candidates_truncated`, `dream.candidates_local`, `dream.candidates`, `dream.dreamer`, `dream.llm_status`, `dream.llm_fallback`, `dream.llm_reject_reason?`, `dream.llm_reject_excerpt?` |
| `dream.llm_guidance` | `llm.ts` (guided arms only; one per iteration `>= 1`) | `dream.run` | `dream.iteration`, `dream.pool_size`, `dream.tokens`, `dream.llm_fallback` |

Outside `core/dream`, `rlm.run_agent` (`core/agent-session.ts`) carries `rlm.requested_thinking`,
`rlm.max_output_tokens` and the effective `rlm.thinking` for every dream child call.

Also owed in `packages/ai/src/trace-context.ts` (off-limits here): add `"dream.run"`, `"dream.dream"`,
`"dream.replay"`, `"dream.redeploy"`, `"dream.experiment"` to `ACTIVE_OPERATION_SPANS` so the long /
detached-root spans emit `span_start` and not only `span_end`. Until then the always-ending sync CLI
spans still log `span_end`, which is sufficient to reconstruct a CLI trace.
