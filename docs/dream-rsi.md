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

### 2. Replay simulator (freeze the tree, zero execution)

A recorded tree is a frozen simulator. An alternative policy re-walks it: each round it picks a
batch of eligible nodes and replay **deterministically** returns the recorded child of each selected
node — the earliest-created not-yet-revealed child for the root, the unique unrevealed recorded child
for a non-root leaf. Nothing new is generated, so thousands of policies are scored at zero cost.

A legally selected cell with **no** unrevealed recorded child (the root once all its children are
revealed, or a non-root leaf that never had a recorded child) reveals nothing and increments
`outOfSupportRounds`. This is the off-policy penalty: a policy that would explore branches the
history never recorded collects less value, because it gains no new nodes and a worse
parallelism ratio.

### 3. Dreaming policy improvement

From the current policy `π⁰`, produce `M` revised policies. Each is scored on every recorded tree by
the replay objective, and the best — which always includes the current policy in the candidate set —
is redeployed online to expand the pool.

The replay objective for policy `m` on tree `i` keeps the structure of the paper's eq. 1 but every
term is dimensionless (`core/dream/objective.ts`):

```
q_m_i   = clamp((max_{v in revealed} s_v − poolMin) / (poolMax − poolMin), 0, 1)
V_m_i   = q_m_i  −  beta1 · N_m_i / (W · k1)  +  beta2 · N_m_i / (max(1, rounds_m_i) · W)
```

- **Term 1** — best quality, normalized against the **pool's** observed valid-score range
  (`poolScoreScale`: min and max over every valid node of every tree in the frozen pool, roots
  included). When the pool has a single score the term is 1 at or above it and 0 below.
- **Term 2** — probe cost: `N_m_i` (revealed non-root nodes) as a fraction of the per-rollout budget
  `W · k1`, so spending the whole budget costs exactly `beta1` quality points.
- **Term 3** — parallelism bonus: the mean batch fill `N / (rounds · W)`, in `[0, 1]`, so a replay
  that fills every worker slot every round earns exactly `beta2`.

The defaults are `beta1 = beta2 = 0.05`. Spending the whole budget at full parallelism therefore
scores exactly `q`, an idle slot or an unspent probe moves `V` by at most `0.05` in total, and a
quality difference of 5% of the pool range always dominates the cost terms. `--beta1` / `--beta2`
(and `ReplayObjectiveConfig` on every core entry) change them, and the experiment result records the
values it ran with.

**Why the normalization.** The paper tunes `beta1` per domain because its penalty is on raw scores.
This codebase shipped with `V = best − 0.01 · N + 0.02 · N / rounds` and on circle-packing (pool
range 0.48) a 36-probe rollout paid 0.36 while the real score gains were about 0.1, so the dreamer
was rewarded for frugality far more than for quality and "improved" by learning to stop exploring:
seed 7, six rounds, dream-arm probes 36, 12, 1, 1, 4, 1 with the best frozen at round 1 (1.1406),
against the fixed-exploration control's 36, 38, 38, 38, 37, 32 reaching 1.2581. Replaying the
recorded trees fixes the defaults: on the control's six trees the exploring policy reaches mean best
1.1777 with 36.5 probes and the collapsed one 0.9617 with 1 probe, yet the old objective ranked the
collapsed policy higher (0.9717 vs 0.8744); normalized, exploring wins 0.8343 to 0.3985. The
break-even `beta1` (at `beta2 = 0.05`) is 0.639 on circle-packing and 0.109 on python-speedup's
control pool (0.087 on its dream pool, whose improvements cluster within 4% of the range), so 0.05
keeps a 12.8x, 2.2x and 1.7x margin, whereas 0.1 is a coin flip on python-speedup (0.9478 vs 0.9425
on its control pool, and it loses 0.9759 to 0.9782 on its dream pool). `test/dream-improve.test.ts`
pins both the documented bug (the raw formula is kept there as a literal) and the fix on two of the
recorded trees under `test/fixtures/dream/`.

A policy's pool score is the arithmetic mean of `V` over the recorded trees, in a deterministic
order (sorted by tree id), together with the mean `q`. Selection is the argmax over `{current} ∪
candidates` restricted to the candidates whose mean `q` is **at least the current policy's** (within
`qualityEps`, default 0); the current policy is always in the set and wins ties. Because `V` is a
pure function of the frozen history, its scale and the budget, the selected policy is provably **no
worse than the current one on replay in `V` and never lower in quality**: a candidate cannot win by
collapsing exploration, only by reaching at least the same best for less. The guard is what saves
python-speedup at aggressive betas (its collapsed policy's mean `q` 0.9782 sits below the exploring
0.9821 on the dream pool) and it forbids the recorded step-1 collapse on circle-packing under any
beta (mean `q` 1.0 → 0.47). It is an off-policy estimate: a policy that would explore un-recorded
branches is out of support and earns nothing there.

## The shared decision interface

Both online and replay drive the **same** policy interpreter through one `ObservationView`:
`maxParallelism` (`W`), `round`, `observed()`, `legalActions()`, `legalRoots()`, `bestScore()`, and
`revealedNonRootCount()`. Online builds a `LiveObservation` over the growing tree; replay builds a
`ReplayObservation` over the recorded tree plus a revealed-id set. `interpretPolicy(policy, view)`
consumes only the view (the default rules are deterministic and use no RNG), so the identical policy
JSON drives both phases.

A batch is legal when its cells are distinct, all currently legal, at most `W`, and never contain
both a node and its child. `assertLegalBatch(view, cells)` enforces this; the interpreter pre-filters
so it never emits an illegal batch, and the drivers assert it defensively.

## The exploration policy is data, never code

The soundness invariant of the whole subsystem: **a policy is a flat JSON object of numbers and
named-rule string literals, never code.** A fixed TypeScript interpreter is the only thing that acts
on it. `parseExplorationPolicy` rejects any unknown key, any out-of-range number, any non-integer
where an integer is required, and any named rule outside `SELECTION_RULES` / `RECOVERY_POLICIES` /
`STOP_RULES`, throwing `PolicyValidationError`. A stringified function or code payload is rejected as
an unknown-key / type error. There is no `eval`, no `Function`, no child process, and no dynamic
import of policy content anywhere.

The policy fields include `branchWidth`, `refineDepth`, `batchSize`, `beta` (stopping patience),
`promisingThreshold`, a `recoveryPolicy`, and a small set of named building-block rules. Dreaming
optimizes these fields and structure — by local search / evolution over the parameter space by
default, or optionally an LLM that emits a **constrained policy JSON that is parsed through the same
validator**, so a bad LLM policy can never regress the deployed one and never executes.

## Determinism

All randomness flows through one injected `SeededRng` (splitmix64 over a masked 64-bit BigInt state,
forkable by hashing `seed + label`, each fork independent of the parent's draws) and one injected
`DreamClock = () => number`. The tree, replay and objective core never call `Date.now`, `Math.random`
or `randomUUID`. Replay uses no RNG at all (deterministic ranking). Node ids are
`<treeId>-n<seq>`; the only clock-derived id is the `treeId` at the outer boundary. Two runs with the
same seed into two fresh stores produce identical tree ids, identical final policy ids and scores,
and byte-identical tree files.

## Persistence (JSONL + blobs)

The store mirrors the RAVO archive layout. The directory is `PRIME_AGENT_DREAM_DIR` if set (tilde
expanded), else `<agent-dir>/dream`. Trees are `<dreamDir>/trees/<treeId>.jsonl`; full artifacts are
`<dreamDir>/trees/<treeId>/blobs/<seq>.json`, so node lines stay scalar-only.

- **Header** (line 0): `{type:"tree",version,treeId,taskId,n?,w,seed,policyId,iteration,createdTs}`.
- **Node line**: `{type:"node",id,parentId,branch,seq,round,score,valid,failClass?,artifactRef,tokens,ts}`
  with `id = <treeId>-n<seq>` and `ts` from the injected clock.
- **Reveal line** (one per online round): `{type:"reveal",round,ids:[...]}` — informational for
  `show`; replay derives reveals from `seq` + `parent` and does not read it.

The header is written with a create/truncate so a same-seed re-run is idempotent; node and reveal
lines are appended. Scores persist as `valid:boolean` plus a **finite** `score:number` (0 when
invalid) — never `Infinity`/`NaN`, which `JSON.stringify` would turn into `null`. Eligibility and the
objective gate on `valid`.

## The tasks

The two numeric tasks are pure, deterministic, and need no GPU or external API; the code task is a
real bounded subprocess.

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
`simulate` → replay, `compare` → experiment, `inspect` → show. Common flags: `--task`, `--n`,
`--seed` (default 1), `--workers` (`W`, default 4), `--k1` (default 12), `--k2` (default 24),
`--dreams` (`M`, default 16), `--beta1` / `--beta2` (the replay objective's weights, finite and
non-negative, default 0.05 each), `--iterations` (default 3), `--tree <id|latest>`, `--dir <path>`,
`--json`, `--llm-proposer`, `--llm-dreamer`. `experiment` takes `--rounds` (default 4), `--arms`
(default `dream,fixed`), `--seeds a,b,c` and `--overwrite` instead of `--iterations`. `replay`,
`improve`, `loop` and `experiment` all score with `--beta1` / `--beta2`, and `replay` and `improve`
measure the cost term against `--k1` (the budget is the tree's `W` times `k1`).

| subcommand | what it does | span opened |
|---|---|---|
| `loop` (default) | rollout → dream → redeploy for `--iterations`; prints per-round best score and probes (paper Fig. 6), initial vs final policy, best node score, tree ids, tokens (0 locally) | `dream.run` wrapping `dream.explore` / `dream.dream` / `dream.redeploy` |
| `experiment` (`compare`) | runs every `--arms` arm from the same initial policy, seed, clock and budget for `--rounds` rollouts each, the `fixed` arm never dreaming; prints a per-arm round table and the headline card, writes `experiments/<id>/result.json` (see [Experiments](#experiments-the-fixed-exploration-control)) | `dream.experiment` wrapping one `dream.experiment_arm` → `dream.run` per arm |
| `rollout` (`propose`) | one online exploration with the default policy; persists the tree; prints tree id, rounds, revealed count, best score, best node id | `dream.explore` |
| `replay` (`simulate`) | loads a recorded tree (`--tree`), re-walks it with the default policy, prints the `ReplayResult` and `V`; zero execution | `dream.replay` (root) |
| `improve` | freezes the task's tree pool, runs the local dreaming search, prints chosen vs current policy and candidate ids | `dream.dream` |
| `status` | read-only store summary: tree count, per-task counts, best node score, last policy id, store dir | none |
| `show` (`inspect`) | prints one tree's header, per-round reveals, and each node | none |

`--json` prints the result object as JSON. Exit codes: `0` success; `1` a usage error (bad or unknown
flag, or `--n` outside the accepted range); `2` an LLM flag on the standalone CLI, or a read
subcommand (`status`/`replay`/`show`/`improve`) run against an empty store or pool.

### Why the LLM flags are rejected

The zero-token guarantee is structural. The barrel the CLI imports (`core/dream/index.ts`) does not
re-export `llm.ts`; `dream-command.ts` imports only `core/dream/index.js`; and the `--llm-*`
rejection is pure argv inspection. `llm.ts` (which reaches the child-agent call path via
`createRunAgentChildCall`) is only reachable from in-session callers that hold a `RunAgentHandler`.
The standalone CLI has none, so `--llm-proposer` / `--llm-dreamer` return exit 2 with:

```
LLM proposer/dreamer require an in-session agent handler and are unavailable from the standalone CLI; the default local proposer runs at zero tokens.
```

No token is spent and no socket is opened.

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
  input to the multipliers.
- **Handler calls** per role (`proposer`, `dreamer`, `guidance`; retries included) and child
  **tokens** are cost. They are recorded and plotted as cost and never mixed into the compute axis.
  Zero on the local path.

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

### Stores and result

Each arm gets its own store, `<dream dir>/experiments/<experimentId>/<arm>/trees/`, so an arm's
frozen pool holds only its own trees; `listTrees`, `freezePool` and `dream status` read
`<dream dir>/trees` only and never see an experiment. The experiment id is
`<task>-s<seed>-n<rounds>-<clock>`; an existing result is refused unless `--overwrite`.

The result is `<dream dir>/experiments/<experimentId>/result.json`, schema
`prime-agent.dream.experiment/1`: the spec (task, `n`, seed, rounds, budget, initial policy id,
proposer/dreamer mode, model), one entry per arm with `storeDir` (relative to the dream dir),
`runId`, `initialPolicyId` / `finalPolicyId`, the policy's score on the arm's own final pool, the
per-round rows (`treeId`, `policyId`, `roundBest`, `cumulativeBest`, probes and cumulative probes,
per-role handler calls, tokens, the `dreaming` record or null), the arm totals, the `headline`
block, `sharedInitialRollout`, `createdTs` and `notes`. Every number is finite; an undefined value
is `null`, never `NaN`. On the local path the whole file is JSON-equal across repeat runs with the
same seed and clock (`storeDir` is relative), every tree file is byte-identical, and the fixed
arm's round-`i` tree id equals the dream arm's. Two runs of the same seed under different clocks
(two invocations of `dream experiment --seed 7`) differ only in the clock-bearing identity — the
experiment id, run ids, tree ids and timestamps — and print identical round tables, policies and
headline: the clock never reaches an rng stream or an ordering. `python-speedup` is the one
exception, for score values only: its `evaluate` is wall-clock timed, and the result's `notes` say
so. Every result also carries the note `objective: normalized (q in pool range, cost in budget
fractions)`, which distinguishes it from a file scored by the raw-scale objective it replaced.

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
7. Fig. 6b adaptivity is not a multiplier: it is the per-round probes series plus `policyChanges`,
   the number of rounds whose `policyId` differs from the previous round's. The fixed arm's series
   is flat in expectation.
8. Guidance ablation: for each (unguided, guided) pair that ran, `P_guided(N) - P_unguided(N)` and
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
prime-agent dream experiment --task python-speedup --rounds 5 --seed 1 --json
```

`--seeds` runs one experiment per seed and prints each result path. A guided arm on the standalone
CLI exits 2 with `ExperimentArmUnavailableError` and writes nothing; `--llm-proposer` /
`--llm-dreamer` stay rejected as before. In-session, `/dream experiment --task <id> --rounds N
--arms dream,fixed,dream-guided --llm-proposer` and the kernel's `await dream.experiment(task,
rounds, arms=..., llm_proposer=True)` run the same experiment through `DreamRunService
.startExperiment`, with progress in the Agents View (`dream_run_update` gains optional `kind`,
`experimentId`, `arm`, `armIndex`, `armCount`, `round`, `rounds`, `cumulativeProbes` and
`resultPath` fields, an additive change with no protocol bump). Every non-local arm spends tokens.

### Plots

```
python3 evals/dream/plot_experiment.py <dream dir>/experiments/<id>/result.json [--out <dir>] [--check]
```

Several result files are seeds of one experiment. The script writes `round_best.png` (Fig. 6a),
`compute.png` (Figs. 3b/5, with `T` and `B` as guide lines), `attempts.png` (Fig. 6b, policy changes
marked), `headline.png` (the multipliers or the literal words `not reached` / `not comparable`) and
a self-contained `report.html` whose captions come from the result metadata and notes. `--check`
prints the reduced tables without matplotlib and verifies the file's headline arithmetic against
the rounds (`headline recomputed ... agrees with the file: yes`). `evals/dream/README.md` has the details, including the interpreter fallback when the
system `python3` lacks matplotlib.

## In-session vs standalone spans

The sync CLI drivers open `dream.run` and, inside it, `dream.explore` / `dream.dream` /
`dream.redeploy` as direct children — `withSpan` restores the parent context when each sync child
returns, so no child outlives its parent and there are no detached roots. The async in-session
drivers (`runDreamLoopWithAgent`, in `llm.ts`) run dreaming past the user turn, so they mint
`dream.dream` / `dream.redeploy` as **detached roots** via `startSpan` after leaving the ambient
context (the `refine.plan` pattern), each wrapped in `try/finally` so the span ends on every path.
The CLI never needs this.

## Observability rows

These spans are opened under `packages/coding-agent/src/core/dream/*`. All attribute values are
scalars (`string | number | boolean | undefined`) and every span ends on throw and abort. The
authoritative rows are in `docs/observability.md`; this table is the per-file map:

| span | opener | parent | attrs |
|---|---|---|---|
| `dream.experiment` | `experiment.ts` `runExperiment` (sync, in-turn root) / `experiment-llm.ts` `runExperimentWithAgent` (detached root carrying the launching turn's `trigger.trace_id`) | root | `dream.experiment_id`, `dream.task`, `dream.seed`, `dream.rounds`, `dream.arms`, `dream.mode`, `dream.stopped` (`aborted`) |
| `dream.experiment_arm` | `experiment.ts` / `experiment-llm.ts` | `dream.experiment` | `dream.experiment_id`, `dream.arm`, `dream.fixed_policy`, `dream.guided`, `dream.run_id` |
| `dream.run` | `loop.ts` `runDreamLoop` | root of a CLI loop; under an experiment a child of `dream.experiment_arm` (sync) or a detached root whose `trigger.trace_id` is the experiment's trace (async) | `dream.task`, `dream.seed`, `dream.workers`, `dream.k1`, `dream.k2`, `dream.dreams`, `dream.iterations`, `dream.mode`, `dream.fixed_policy` |
| `dream.explore` | `explore.ts` `runOnlineExploration` | `dream.run` | `dream.tree_id`, `dream.policy_id`, `dream.k1`, `dream.workers`, `dream.iteration` |
| `dream.round` | `explore.ts` | `dream.explore` | `dream.round`, `dream.batch_size`, `dream.revealed_count`, `dream.best_score` |
| `dream.attempt` | `explore.ts` | `dream.round` | `dream.node_id`, `dream.parent_id`, `dream.task`, `dream.valid`, `dream.score`, `dream.tokens`, `dream.fail_class?` |
| `dream.replay` | `replay.ts` `simulatePolicyWithSpan` (standalone root; bare `simulatePolicy` opens none so it can run in a tight dreaming loop) / `improve.ts` `runDreaming` (one coarse per-step child of `dream.dream`) | root or `dream.dream` | `dream.policy_id`, `dream.tree_id?`, `dream.revealed_n`, `dream.rounds`, `dream.v`, `dream.out_of_support`, `dream.simulations?` |
| `dream.dream` | `improve.ts` `runDreaming` | `dream.run` (sync) / detached root (async) | `dream.candidates`, `dream.pool_size`, `dream.chosen_policy_id`, `dream.chosen_score`, `dream.current_score`, `dream.chosen_quality`, `dream.current_quality`, `dream.quality_rejected`, `dream.improved` |
| `dream.redeploy` | `loop.ts` | `dream.run` (sync) / detached root (async) | `dream.explore` attrs plus `dream.policy_id`, `dream.fixed_policy` |
| `dream.llm_guidance` | `llm.ts` (guided arms only; one per iteration `>= 1`) | `dream.run` | `dream.iteration`, `dream.pool_size`, `dream.tokens`, `dream.llm_fallback` |

Also owed in `packages/ai/src/trace-context.ts` (off-limits here): add `"dream.run"`, `"dream.dream"`,
`"dream.replay"`, `"dream.redeploy"`, `"dream.experiment"` to `ACTIVE_OPERATION_SPANS` so the long /
detached-root spans emit `span_start` and not only `span_end`. Until then the always-ending sync CLI
spans still log `span_end`, which is sufficient to reconstruct a CLI trace.
