# evals — does the harness actually improve?

A black-box benchmark for Prime Agent **the harness**, not the model. It answers one question with a
number instead of an argument: *after the agent has run for a while, does it solve the same task in
fewer turns, fewer tokens, and with less human instruction than it did cold?*

This lives outside `packages/` on purpose. A benchmark is not core logic, and core must not grow a
dependency on one. See "Why this is separate" below.

## The measurement

Improvement is not "the model seems better." It is a delta on a fixed suite between two runs, with a
real **train/test split** so a warm pass cannot be memorisation of the exact prompt it was corrected on:

| phase | agent state | tasks it runs | what it measures |
|---|---|---|---|
| **A — cold** | empty `PRIME_AGENT_CODING_AGENT_DIR` | the **test** tasks | baseline capability |
| **B — study** | refinement on, state persisted | the **train** tasks only | the harness learns |
| **C — warm** | the state produced by B | the **test** tasks | capability after learning |
| **A' — control** | a *second* cold run | the **test** tasks | run-to-run variance |

A task tagged `study` is a training task and runs **only** in `--phase study`; every other task is a
held-out test task and runs **only** in `--phase cold|warm`. `run.mjs` enforces this: phase study can
never touch a test task, and cold/warm can never touch a study task. Train and test share a *lesson*
(checkpoint your work; don't push unasked) but differ in *surface* — the study tasks are a JavaScript
off-by-one in a subdirectory; the test task is a Python off-by-one at the root.

`improvement = (C − A)`. Whether it is real is decided by a paired permutation test and a bootstrap
CI over repeated runs (see **Statistics**), not by one number beating another. Run each phase with
`--repeat 5` or more.

## What counts as better

Per task, in priority order:

1. **pass** — `verify.sh` exits 0. A faster failure is not an improvement. Invalid runs (a run that
   never happened) are excluded, never scored as a fail.
2. **human interventions** — how many corrections the operator had to deliver *because the task was
   still failing*. A task may declare `followups`; a `when:"fail"` followup is sent only while
   `verify.sh` still fails and counts as one intervention, so a harness that has learned needs zero.
   This is the one that matters most and the one nobody measures.
3. **turns** — assistant turns to reach the verified state.
4. **tokens** — reported per component: uncached input, output, cache-read and cache-write, each
   counted **once per assistant message**. The real prompt (injected memories, recall, skills) lives
   in the cache fields, so a memory that bloats context shows up here.
5. **wall-clock** — last, because it is mostly provider latency.

## Statistics

`report.mjs` drops invalid rows (and prints how many), then for every metric computes:

- a **paired-by-task effect**: the mean over tasks of `mean(after) − mean(base)`;
- a two-sided **permutation p-value** (labels reshuffled within each task's pooled reps);
- a **bootstrap 95% CI** (resampling tasks, and reps within tasks);
- **Holm-Bonferroni** correction across the six metrics.

The verdict is the Holm-adjusted permutation p (`< 0.05` ⇒ better/worse); the CI is shown as the
effect's magnitude, and a "CI straddles 0" note flags a significant p whose tasks disagree. There is
no `|C − A| > |A' − A|` rule any more — a single control run cannot estimate variance, and that rule
was a coin flip at n=1. A control file, if supplied, is shown only as a reference arm: if base-vs-control
lands a verdict, the suite is too noisy to trust base-vs-after.

## Seed tasks come from the corpus, not from imagination

The first tasks are drawn from measured repetition across 9,547 opencode sessions and 241 prime-agent
sessions. The single clearest case:

> `git-checkpoint-preference` — the operator asked for "git diff, stage, commit and push" **118 times
> across ~95 distinct sessions**. That instruction is already stored as harness memory
> `preferences/version-control` (`source: refine`, 2026-08-19) and it ranks **#2 of 27**, so it is
> rendered into *every* system prompt. It is in context, and the operator still has to ask.

That is the benchmark in one line: **a stored, rendered, correct memory that does not change
behaviour is not learning.** If a harness change is real, this task's pass rate goes from 0 (cold,
which never commits without being told) to 1 (warm), and the study tasks that teach it need one
correction, not a per-session repeat.

## Layout

```
tasks/<id>/
  task.json     { id, prompt, timeoutMs, maxTurns, tags, why, followups? }
  setup.sh      optional — prepares the workspace (cwd = workspace, $EVAL_REMOTE = a per-run bare remote)
  verify.sh     required — exit 0 = pass (cwd = workspace; reads .eval/events.jsonl for behaviour)
```

`verify.sh` is the referee. It is a real executable check, never a model judging a model — the same
rule `docs/rsi-plan.md` milestone 3 applies inside the gate. Behavioural checks (did it use the
published skill, run the tests, repeat a command, attempt a push) read `.eval/events.jsonl`, the
folded `--mode json` event stream, and key on structured `tool_execution_start` events rather than
grepping the raw text.

## Running

```sh
node evals/run.mjs --phase cold  --repeat 5 --out evals/results/cold.json
node evals/run.mjs --phase cold  --repeat 5 --out evals/results/cold2.json   # control
node evals/run.mjs --phase study --repeat 5 --state /tmp/warm-state          # train tasks only
node evals/run.mjs --phase warm  --repeat 5 --state /tmp/warm-state --out evals/results/warm.json
node evals/report.mjs evals/results/cold.json evals/results/warm.json --control evals/results/cold2.json
```

To measure the uncommitted working tree rather than the installed global build, point `--bin` at a
built checkout; `report.mjs` prints the version recorded in each file and flags a mismatch.

### Hermeticity and provenance

- A fresh workspace per task, `PRIME_AGENT_CODING_AGENT_DIR` at a temp dir, and `--print --mode json`
  so nothing waits on a human. Only `auth.json` and `settings.json` are copied forward.
- setup, agent, followups and verify all run under a **controlled `GIT_CONFIG_GLOBAL`** (no GPG
  signing, no global hooks, empty excludesfile, `push.autoSetupRemote off`) and
  `PYTHONDONTWRITEBYTECODE=1`, so a score never depends on the operator's `~/.gitconfig`,
  `~/.gitignore_global`, a GPG agent, or stray `__pycache__`.
- The no-push tasks get `$EVAL_REMOTE`, a bare remote the runner creates per run, never a shared
  `/tmp/remote.git`.
- Every result file carries a `provenance` block: `prime-agent --version`, the resolved binary, the
  model, argv/flags, an env allowlist (secrets redacted), a settings hash, the seed source and hash,
  and the runner's git sha + dirty flag. Runs are reproducible and auditable after the fact.

The runner never modifies your live `~/.prime/agent` state; note, however, that in `--mode json` the
agent still executes through your daemon supervisor and shares the real kernel venv — full process
isolation (a private daemon socket and venv) is a known gap tracked against the runner, not something
these result files claim.

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
