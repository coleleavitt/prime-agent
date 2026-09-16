# evals — does the harness actually improve?

A black-box benchmark for Prime Agent **the harness**, not the model. It answers one question with a
number instead of an argument: *after the agent has run for a while, does it solve the same task in
fewer turns, fewer tokens, and with less human instruction than it did cold?*

This lives outside `packages/` on purpose. A benchmark is not core logic, and core must not grow a
dependency on one. See "Why this is separate" below.

## The measurement

Improvement is not "the model seems better." It is a delta on a fixed suite between two runs:

| phase | agent state | what it measures |
|---|---|---|
| **A — cold** | empty `PRIME_AGENT_CODING_AGENT_DIR` | baseline capability |
| **B — study** | training tasks, refinement on, state persisted | the harness learns |
| **C — warm** | the state produced by B | capability after learning |
| **A' — control** | a *second* cold run | run-to-run variance |

`improvement = (C − A)`, and it only counts if `|C − A| > |A' − A|`. Without the control you are
reading noise.

## What counts as better

Per task, in priority order:

1. **pass** — `verify.sh` exits 0. A faster failure is not an improvement.
2. **human interventions** — how many times the operator had to say anything beyond the first prompt.
   Zero is the target. This is the one that matters most and the one nobody measures.
3. **turns** — assistant turns to reach the verified state.
4. **tokens** — input + output.
5. **wall-clock** — last, because it is mostly provider latency.

## Seed tasks come from the corpus, not from imagination

The first tasks are drawn from measured repetition across 9,547 opencode sessions and 241 prime-agent
sessions. The single clearest case:

> `git-checkpoint-preference` — the operator asked for "git diff, stage, commit and push" **118 times
> across ~95 distinct sessions**. That instruction is already stored as harness memory
> `preferences/version-control` (`source: refine`, 2026-08-19) and it ranks **#2 of 27**, so it is
> rendered into *every* system prompt. It is in context, and the operator still has to ask.

That is the benchmark in one line: **a stored, rendered, correct memory that does not change
behaviour is not learning.** If a harness change is real, this task's intervention count goes to zero.

## Layout

```
tasks/<id>/
  task.json     { id, prompt, timeoutMs, maxTurns, tags, why }
  setup.sh      optional — prepares the workspace (cwd = workspace)
  verify.sh     required — exit 0 = pass (cwd = workspace)
```

`verify.sh` is the referee. It is a real executable check, never a model judging a model — the same
rule `docs/rsi-plan.md` milestone 3 applies inside the gate.

## Running

```sh
node evals/run.mjs --phase cold  --out evals/results/cold.json
node evals/run.mjs --phase cold  --out evals/results/cold2.json     # the control
node evals/run.mjs --phase study --state /tmp/warm-state
node evals/run.mjs --phase warm  --state /tmp/warm-state --out evals/results/warm.json
node evals/report.mjs evals/results/cold.json evals/results/warm.json --control evals/results/cold2.json
```

Every run is hermetic: a fresh workspace per task, `PRIME_AGENT_CODING_AGENT_DIR` pointed at a temp
dir, and `--print --mode json` so nothing waits on a human. The runner never touches your live
`~/.prime/agent`.

## Why this is separate

A specific external benchmark should not be a member of a production gate's type union, and
`core/ravo/arc-agi-evaluator.ts` shells out to `uv run main.py` in a cloned third-party repo. That is
a benchmark leaking into core.

**Step one is done.** The evaluator is now an interface —
`core/ravo/external-evaluator.ts:ExternalEvaluatorSuite` — and `run-service.ts` talks only to that.
The roughly 220 lines of ARC-specific gate wiring that used to live in `run-service.ts` (the outcome
opponents `arc:no-crash` / `arc:all-levels`, the `uv` fast screen, the `agents/agent.py` prompt
reference, the committed-agent persistence) moved into `core/ravo/arc-agi-evaluator.ts` behind one
factory, `createArcEvaluatorSuite`. `run-service.ts` now names ARC in exactly four places: one import,
the `evaluator` union member, the `arcRunner` test seam, and the single call that builds the suite.

**Step two is not done, and needs a wider change than a file move.** ARC cannot simply be relocated
into `evals/` today:

- `run-service.ts` does not merely reference an ARC *type*; it must construct the suite. Whoever
  constructs it has to be inside the agent process.
- Two more product files carry ARC surface: `core/slash-commands.ts` parses `--arc-repo` /
  `--arc-game`, and `core/agent-session.ts` parses the `arc_agi` payload of the `ravo.run` host
  request. Both produce `{ kind: "arc-agi", repoDir, game }` values, so the union member cannot be
  dropped without editing them in the same change.
- `evals/` is outside `tsconfig.json`'s `include` and outside `biome.json`'s `files.includes`, and it
  has no test runner. A `.ts` file moved here today is not type-checked, not linted, and not tested —
  an orphan, not a module. `RavoRunService` is also not in the package's `exports` map, so nothing
  here could import it even if it were checked.

So the remaining move is: delete `core/ravo/arc-agi-evaluator.ts`, delete its four references in
`run-service.ts`, drop the `arc-agi` union member plus the CLI flags and the host-request field, and
re-introduce ARC as an `ExternalEvaluatorSuite` supplied through a runtime extension point (an
extension or a skill) rather than a TypeScript import. That is milestone 3's `{kind:"replay"}` work,
not a `git mv`.
