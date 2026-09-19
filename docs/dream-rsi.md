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

The replay objective for policy `m` on tree `i` (eq. 1) is:

```
V_m_i = max_{v in revealed} s_v  −  beta1 · N_m_i  +  beta2 · N_m_i / max(1, rounds_m_i)
```

- **Term 1** — best quality: the maximum valid score over revealed nodes.
- **Term 2** — probe penalty: `N_m_i` is the number of revealed non-root nodes (probes spent).
- **Term 3** — parallelism bonus: attempts per decision round.

A policy's pool score is the arithmetic mean of `V` over the recorded trees, in a deterministic
order (sorted by tree id). Because the current policy is always in the candidate set and wins ties,
and `V` is a pure function of the frozen history, the selected policy is provably **no worse than the
current one on replay**. It is an off-policy estimate: a policy that would explore un-recorded
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

Both tasks are pure, deterministic, numeric, and need no GPU or external API.

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

## The CLI

```
prime-agent dream [rollout|replay|improve|loop|status|show] [options]
```

The subcommand is the first positional; absent, it defaults to `loop`. Aliases: `propose` → rollout,
`simulate` → replay, `inspect` → show. Common flags: `--task`, `--n`, `--seed` (default 1),
`--workers` (`W`, default 4), `--k1` (default 12), `--k2` (default 24), `--dreams` (`M`, default 16),
`--iterations` (default 3), `--tree <id|latest>`, `--dir <path>`, `--json`, `--llm-proposer`,
`--llm-dreamer`.

| subcommand | what it does | span opened |
|---|---|---|
| `loop` (default) | rollout → dream → redeploy for `--iterations`; prints per-round best score and probes (paper Fig. 6), initial vs final policy, best node score, tree ids, tokens (0 locally) | `dream.run` wrapping `dream.explore` / `dream.dream` / `dream.redeploy` |
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

## In-session vs standalone spans

The sync CLI drivers open `dream.run` and, inside it, `dream.explore` / `dream.dream` /
`dream.redeploy` as direct children — `withSpan` restores the parent context when each sync child
returns, so no child outlives its parent and there are no detached roots. The async in-session
drivers (`runDreamLoopWithAgent`, in `llm.ts`) run dreaming past the user turn, so they mint
`dream.dream` / `dream.redeploy` as **detached roots** via `startSpan` after leaving the ambient
context (the `refine.plan` pattern), each wrapped in `try/finally` so the span ends on every path.
The CLI never needs this.

## Owed observability rows

These spans are opened under `packages/coding-agent/src/core/dream/*`. All attribute values are
scalars (`string | number | boolean | undefined`) and every span ends on throw and abort. They are
**not yet folded into `docs/observability.md`** (that file is owned elsewhere); list for the docs
owner:

| span | opener | parent | attrs |
|---|---|---|---|
| `dream.run` | `loop.ts` `runDreamLoop` | root of a CLI loop | `dream.task`, `dream.seed`, `dream.workers`, `dream.k1`, `dream.k2`, `dream.dreams`, `dream.iterations`, `dream.mode` |
| `dream.explore` | `explore.ts` `runOnlineExploration` | `dream.run` | `dream.tree_id`, `dream.policy_id`, `dream.k1`, `dream.workers`, `dream.iteration` |
| `dream.round` | `explore.ts` | `dream.explore` | `dream.round`, `dream.batch_size`, `dream.revealed_count`, `dream.best_score` |
| `dream.attempt` | `explore.ts` | `dream.round` | `dream.node_id`, `dream.parent_id`, `dream.task`, `dream.valid`, `dream.score`, `dream.tokens`, `dream.fail_class?` |
| `dream.replay` | `replay.ts` `simulatePolicyWithSpan` (standalone root; bare `simulatePolicy` opens none so it can run in a tight dreaming loop) / `improve.ts` `runDreaming` (one coarse per-step child of `dream.dream`) | root or `dream.dream` | `dream.policy_id`, `dream.tree_id?`, `dream.revealed_n`, `dream.rounds`, `dream.v`, `dream.out_of_support`, `dream.simulations?` |
| `dream.dream` | `improve.ts` `runDreaming` | `dream.run` (sync) / detached root (async) | `dream.candidates`, `dream.pool_size`, `dream.chosen_policy_id`, `dream.chosen_score`, `dream.current_score`, `dream.improved` |
| `dream.redeploy` | `loop.ts` | `dream.run` (sync) / detached root (async) | `dream.explore` attrs plus `dream.policy_id` |

Also owed in `packages/ai/src/trace-context.ts` (off-limits here): add `"dream.run"`, `"dream.dream"`,
`"dream.replay"`, `"dream.redeploy"` to `ACTIVE_OPERATION_SPANS` so the long / detached-root spans
emit `span_start` and not only `span_end`. Until then the always-ending sync CLI spans still log
`span_end`, which is sufficient to reconstruct a CLI trace.
