# RAVO ARC-AGI-3 evaluator

`packages/coding-agent/src/core/ravo/arc-agi-evaluator.ts` turns one ARC-AGI-3
game into a **deep, outcome-based** evaluator for the RAVO controller
(`core/ravo/controller.ts`). The candidate artifact is a Python `Agent`
subclass for the [arcprize/ARC-AGI-3-Agents](https://github.com/arcprize/ARC-AGI-3-Agents)
harness. The evaluator installs it in a local clone, plays the game for real
through `uv run main.py`, and reads the final scorecard. No LLM is consulted;
the child call spends zero tokens.

## The deep oracle

The Rocq development (`~/RocqProjects/ravo/Ravo.v`, Section 2) models the
deep evaluation as a total function

```
Variable deep : A -> nat.
Definition commitGate (P : Lineage) (x : A) : Lineage :=
  if bestScore P <=? deep x then (x, deep x) :: P else P.
```

and the TypeScript reducer (`ravoStep` in `core/ravo/reducer.ts`) mirrors it:
the deep gate passes iff the observation has `status: "pass"` and a safe
natural `score >= previousBestScore`. Everything the proofs say about the
deep gate (gate safety `bestScore_commitGate`, run-level monotonicity
`ravoRun` invariants, epsilon-succession over the opponent pool) holds for
*any* `deep`, so the controller is free to choose what `deep x` means.

This evaluator instantiates `deep` with a measured outcome:

| Rocq            | ARC-AGI-3 evaluator                                              |
| --------------- | ---------------------------------------------------------------- |
| `A`             | `ArcAgentArtifact = { agentName, source }` (a Python `Agent`)     |
| `deep x`        | `round(100 * levels_completed / total_levels)` for the played game |
| `deep x` defined| the run produced a scorecard and raised no exception (`pass`)     |
| gate bar        | `previousBestScore` in the reducer lineage                        |

The score is a natural number in `[0, 100]`, which is what `isSafeNatural`
in the reducer requires. `pass` means "the outcome was observed"; the *value*
of that outcome is the score, and the ratchet `score >= previousBestScore`
is what accepts or rejects the candidate. A candidate whose agent crashed
inside the harness is reported as `fail` with the score it reached before
crashing; a run that produced no scorecard at all (timeout, `uv` missing,
malformed artifact) is `error`. Both are non-`pass` statuses, so the reducer
rejects with `rejection: "deep"` and the lineage is untouched.

Section 8 of the Rocq development treats `deep` as a *sampled* estimate and
bounds the probability of a false pass across a run. One game per candidate
is exactly one sample of that estimate: the levels-completed score is
deterministic for a deterministic agent on a fixed game version, and noisy
for an agent that uses randomness. Treat `timeoutMs` and the game choice as
the sampling design; running several games per candidate (see "Cost") is how
you shrink the noise, at proportional cost.

## What the evaluator does

`createArcAgiEvaluator({ repoDir, game, timeoutMs?, runner?, id? })` returns
an `EvaluationAdapter<ArcAgentArtifact>` with `kind: "deep"`. For each
candidate it:

1. Validates the artifact: `agentName` matches `/^[a-z][a-z0-9_]*$/` and
   `source` defines exactly one direct `Agent` subclass (`class X(Agent):`).
   `main.py` discovers agents via `Agent.__subclasses__()`, so indirect
   subclasses are not visible to `--agent`.
2. Writes `agents/templates/<agentName>.py` with a
   `# ravo-arc-agi candidate: <agentName>` header. It refuses to overwrite a
   module that does not carry the header, so repo templates such as
   `random_agent.py` are safe.
3. Rewrites one managed block at the end of `agents/__init__.py`:

   ```python
   # >>> ravo-arc-agi managed agent (generated; do not edit)
   from .templates.<agentName> import <ClassName> as _RavoArcAgent_<agentName>
   AVAILABLE_AGENTS["<agentName>"] = _RavoArcAgent_<agentName>
   # <<< ravo-arc-agi managed agent
   ```

   The block is replaced, not appended, so the clone only ever registers the
   candidate under evaluation.
4. Runs `uv run main.py --agent=<agentName> --game=<game>` in `repoDir`
   through the runner (default: `child_process.spawn`, killed with `SIGKILL`
   on timeout or abort). The adapter also enforces `timeoutMs` and the call's
   `AbortSignal` itself, so an injected runner that ignores them cannot hang
   the controller.
5. Parses the `--- FINAL SCORECARD REPORT ---` JSON that `main.py` logs
   (`parseArcScorecard(stdout)`), computes the score
   (`arcScorecardScore`), and maps the run to a verdict (`interpretArcRun`).

Exports: `createArcAgiEvaluator`, `evaluateArcAgent`, `installArcAgent`,
`parseArcScorecard`, `arcScorecardScore`, `interpretArcRun`,
`arcAgentSelector`, `defaultArcRunner`, and the `ArcAgentArtifact`,
`ArcRunner`, `ArcRunnerArgs`, `ArcRunnerResult`, `ArcScorecard`,
`ArcEnvironmentScore`, `ArcEvaluationResult` types.

## Running it

Prerequisites:

- A working clone of ARC-AGI-3-Agents with `uv` and a `.env` containing
  `ARC_API_KEY` and `OPERATION_MODE=normal`. Games are downloaded into
  `environment_files/` on first use and played locally afterwards.
- The candidate source must import `Agent` relatively
  (`from ..agent import Agent`) like the shipped templates.

```ts
import { createArcAgiEvaluator } from "./core/ravo/index.js";

const deep = createArcAgiEvaluator({
	repoDir: "/tmp/arc-agi-3",
	game: "ls20",
	timeoutMs: 5 * 60 * 1000,
});

// inside RavoControllerOptions<ArcAgentArtifact>
evaluators: [fastScreen, deep, ...opponents],
```

The verdict for one candidate looks like:

```json
{ "status": "pass", "score": 0, "detail": "0/7 levels in 41 actions for ls20" }
{ "status": "fail", "score": 0, "detail": "agent raised KeyError: 'deliberate'; 0/7 levels in 0 actions for ls20" }
{ "status": "error", "detail": "ARC-AGI-3 run timed out after 300000ms" }
```

Tests (`test/ravo-arc-agi-evaluator.test.ts`) use an injected fake runner and a
captured scorecard fixture; they never call `uv`. A real end-to-end smoke
lives in `test/ravo-arc-agi-evaluator-smoke.test.ts` and is skipped unless
`ARC_SMOKE=1`:

```sh
cd packages/coding-agent
ARC_SMOKE=1 ARC_AGI_REPO=/tmp/arc-agi-3 ARC_AGI_GAME=ls20 \
  env -u RLM_MAX_DEPTH npx tsx ../../node_modules/vitest/dist/cli.js --run test/ravo-arc-agi-evaluator-smoke.test.ts
```

## Cost

One deep evaluation is **one full game run per candidate**: a `uv` process
start plus up to `MAX_ACTIONS` environment steps. With the local engine and a
fixed-policy agent, `ls20` takes about two seconds end to end; an LLM-backed
agent is bounded by its own per-action latency and spend, none of which is
metered by the controller (`tokens: 0`). The default timeout is ten minutes.

Because the score is a single sample, the evaluator is deliberately cheap to
compose: to average over several games, pass a comma-separated `game`
prefix list (`main.py` plays every matching game in one scorecard, and the
score uses `total_levels_completed / total_levels`), or wrap several adapters
under one deep evaluator. Each extra game multiplies the cost linearly.

The clone is mutated on every evaluation (candidate module plus managed
block), so use a scratch clone per controller run and do not point
`repoDir` at a checkout you care about.
