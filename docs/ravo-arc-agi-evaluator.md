# RAVO ARC-AGI-3 evaluator

> **Not ported.** The Rust build has no ARC-AGI-3 evaluator. This document records the TS fork's design (branch
> `perf/session-catalog-resume`, `packages/coding-agent/src/core/ravo/arc-agi-evaluator.ts`) and why it was built, so
> a port has a spec. It is benchmark code, not product: `pa-ravo` keeps it out deliberately.

## What the Rust build does

- `/ravo` still parses `--arc-repo DIR --arc-game ID` (`parse_ravo_command`, `ArcAgiTarget`, both flags or neither,
  as in `RAVO_USAGE`), then refuses the command: "RAVO's ARC-AGI evaluator (--arc-repo/--arc-game) is not part of this
  build".
- A `ravo.run` host request carrying `arc_agi` is refused by `parse_ravo_run_payload`: "ravo.run arc_agi is not
  available: the ARC-AGI evaluator is not part of this build". The bundled `ravo` skill (`skills/.features/ravo`, the
  TS skill verbatim) still accepts and forwards the `arc_agi` argument, so the call fails at the host.
- Persisted `arc:*` opponent criteria from a TS-era harness state load and keep their weights; `/refine` and a judge
  `ravo.run` treat them as dormant passes.
- The run controller (`run_ravo_controller` in `crates/pa-ravo/src/run/controller.rs`) consults evaluators by role
  (`EvaluatorKind`: `Fast`, `Deep`, `Opponent`). The only deep evaluator is the judge model call. A port would add an
  outcome-based deep evaluator beside it; nothing in the reducer has to change.

## Why it existed: the deep oracle

The Rocq development (~/RocqProjects/ravo/Ravo.v, Section 2) models the deep evaluation as a total function

```
Variable deep : A -> nat.
Definition commitGate (P : Lineage) (x : A) : Lineage :=
  if bestScore P <=? deep x then (x, deep x) :: P else P.
```

and the reducer (`ravo_step` in `crates/pa-ravo/src/reducer.rs`, TS `ravoStep`) mirrors it: the deep gate passes iff
the observation has status pass and a safe natural score that, plus the configured tolerance, reaches the previous
best score. Everything the proofs say about the deep gate (gate safety, bestScore_commitGate; run-level monotonicity;
epsilon-succession over the opponent pool) holds for *any* `deep`, so the controller is free to choose what `deep x`
means. The judge is one choice, and an opinion. The ARC-AGI-3 evaluator was the other: a measured outcome, with no
model consulted.

| Rocq | TS ARC-AGI-3 evaluator |
| --- | --- |
| `A` | `ArcAgentArtifact = { agentName, source }` (a Python `Agent`) |
| `deep x` | `round(100 * levels_completed / total_levels)` for the played game |
| `deep x` defined | the run produced a scorecard and raised no exception (`pass`) |
| gate bar | the previous best score in the reducer lineage |

The score is a natural number in [0, 100], which is what the reducer's safe-integer checks require. `pass` means "the
outcome was observed"; the value of that outcome is the score, and the ratchet against the previous best is what
accepts or rejects the candidate. A candidate whose agent crashed inside the harness was reported as `fail` with the
score it reached before crashing; a run that produced no scorecard at all (timeout, `uv` missing, malformed artifact)
was `error`. Both are non-pass statuses, so the reducer rejects on the deep gate and the lineage is untouched.

Section 8 of the Rocq development treats `deep` as a *sampled* estimate and bounds the probability of a false pass
across a run. One game per candidate is exactly one sample: the levels-completed score is deterministic for a
deterministic agent on a fixed game version, and noisy for an agent that uses randomness. The timeout and the game
choice are the sampling design; running several games per candidate shrinks the noise at proportional cost.

## The TS design

The candidate artifact was a Python `Agent` subclass for the
[arcprize/ARC-AGI-3-Agents](https://github.com/arcprize/ARC-AGI-3-Agents) harness. The evaluator installed it in a
local clone, played the game for real through `uv run main.py`, and read the final scorecard. The child call spent
zero tokens.

`createArcAgiEvaluator({ repoDir, game, timeoutMs?, runner?, id? })` returned a deep evaluation adapter. For each
candidate it:

1. Validated the artifact: `agentName` matches `/^[a-z][a-z0-9_]*$/` and `source` defines exactly one direct `Agent`
   subclass (`class X(Agent):`). `main.py` discovers agents via `Agent.__subclasses__()`, so indirect subclasses are
   not visible to `--agent`.
2. Wrote `agents/templates/<agentName>.py` with a `# ravo-arc-agi candidate: <agentName>` header, refusing to
   overwrite a module without that header, so repo templates such as `random_agent.py` were safe.
3. Rewrote one managed block at the end of `agents/__init__.py`:

   ```python
   # >>> ravo-arc-agi managed agent (generated; do not edit)
   from .templates.<agentName> import <ClassName> as _RavoArcAgent_<agentName>
   AVAILABLE_AGENTS["<agentName>"] = _RavoArcAgent_<agentName>
   # <<< ravo-arc-agi managed agent
   ```

   The block was replaced, not appended, so the clone only ever registered the candidate under evaluation.
4. Ran `uv run main.py --agent=<agentName> --game=<game>` in the clone, killed with SIGKILL on timeout or abort. The
   adapter enforced the timeout and the call's abort signal itself, so an injected runner that ignored them could not
   hang the controller.
5. Parsed the `--- FINAL SCORECARD REPORT ---` JSON that `main.py` logs, computed the score, and mapped the run to a
   verdict.

Verdicts looked like:

```json
{ "status": "pass", "score": 0, "detail": "0/7 levels in 41 actions for ls20" }
{ "status": "fail", "score": 0, "detail": "agent raised KeyError: 'deliberate'; 0/7 levels in 0 actions for ls20" }
{ "status": "error", "detail": "ARC-AGI-3 run timed out after 300000ms" }
```

Prerequisites were a working clone of ARC-AGI-3-Agents with `uv` and a `.env` holding `ARC_API_KEY` and
`OPERATION_MODE=normal` (games download into `environment_files/` on first use and play locally afterwards), and
candidate sources importing `Agent` relatively (`from ..agent import Agent`) like the shipped templates. The TS tests
(`packages/coding-agent/test/ravo-arc-agi-evaluator.test.ts`) used an injected fake runner and a captured scorecard
fixture; a real end-to-end smoke (`packages/coding-agent/test/ravo-arc-agi-evaluator-smoke.test.ts`) ran only with
`ARC_SMOKE=1`.

## Cost

One deep evaluation was one full game run per candidate: a `uv` process start plus up to the harness's action limit.
With the local engine and a fixed-policy agent, `ls20` took about two seconds end to end; an LLM-backed agent is
bounded by its own per-action latency and spend, none of which the controller metered. The default timeout was ten
minutes.

Because the score is a single sample, the evaluator was cheap to compose: a comma-separated `game` prefix list makes
`main.py` play every matching game into one scorecard (the score uses total levels completed over total levels), or
several adapters can sit under one deep evaluator. Each extra game multiplies the cost linearly.

The clone is mutated on every evaluation (candidate module plus managed block), so a port should keep the rule: a
scratch clone per controller run, never a checkout you care about.
