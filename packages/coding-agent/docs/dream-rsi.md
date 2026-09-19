# Dream-RSI

Dream-RSI (Zheng et al., 2026) grows a discovery tree over a scored task,
freezes each tree into a zero-cost replay simulator, and improves a typed,
serializable exploration policy: it rolls out with the default policy, dreams
one or more candidate policies that should score no worse on replay, selects a
no-regression policy, and redeploys it in a fresh rollout. The framework lives
under `src/core/dream/`; the standalone `prime-agent dream` CLI
(`src/cli/dream-command.ts`) drives the synchronous, local, zero-token path.

## Soundness

A dreamed exploration policy is **data**, never code. The LLM dreamer emits a
flat JSON policy that `parseExplorationPolicy` accepts only if every field is a
known key of the right type and in range; `selectBestPolicy` returns the argmax
of `{current} ∪ candidates` over the candidates whose mean replay quality is no
lower than the current policy's, with the current policy winning ties, so a
worse, malformed or exploration-collapsing policy can never regress the
deployed one. The replay objective is scale-invariant (quality normalized to
the pool's score range, cost as a fraction of the `W * k1` probe budget,
parallelism as batch fill; `docs/dream-rsi.md` has the derivation and the
recorded regression that forced it). A proposed artifact is validated by
`task.deserialize` before it enters the tree and re-scored by `task.evaluate`.
Nothing evals, `Function`-constructs, or spawns any child output. Determinism
flows through one injected `SeededRng` and one injected clock: every rng fork
is labelled by seed, iteration, round, parent seq and child slot, never by an
id, so a seed fixes every score and shape while the clock reaches only the
on-disk ids, and a recorded tree replays byte-identically.

Only the `--llm-proposer` / `--llm-dreamer` (kernel: `llm_proposer` /
`llm_dreamer`) paths spend tokens by driving a child coding agent. The default
runs the local zero-token proposer and dreamer and touches no network.

## In-session invocation surfaces

Beyond the CLI, Dream-RSI has four in-session surfaces, mirroring `/ravo`.
They are available only in a depth-0 session where self-improvement is allowed
(a local harness state directory, not an RLM child), so the surfaces stay in
lockstep with where the `dream.*` host handlers are registered.

1. **Model (kernel skill).** The bundled `dream` skill
   (`skills/dream/`) exposes `await dream.run(task=..., iterations=...,
   llm_proposer=False, llm_dreamer=False, ...)`, `await dream.status()`, and
   `await dream.cancel()`. Each call sends a `dream.run` / `dream.status` /
   `dream.cancel` host request handled by `AgentSession.handleDreamHostRequest`.
   `dream.run` returns immediately; the run continues in the background.

2. **Human (`/dream`).** `/dream [--task <id>] [--n N] [--seed N] [--workers N]
   [--k1 N] [--k2 N] [--dreams N] [--iterations N] [--llm-proposer]
   [--llm-dreamer]` is a session slash command parsed by
   `parseDreamCommandOptions` and dispatched in `AgentSession`. It appends a
   durable "started" row and, when the background run settles, a durable
   terminal row ("Dream-RSI run `<id>` completed", or a failure row).

3. **Live progress (`dream_run_update`).** `DreamRunService` pushes a
   `DreamRunStatus` snapshot through `onUpdate` at each phase boundary; the
   session re-emits it as a `dream_run_update` event. The daemon forwards it,
   capability-gated on `dream_run_updates`, to roster subscribers so a run's
   phase, iteration, best node score, and final policy score appear live in the
   Agents View. Each field of `DreamRunStatus` is a scalar.

4. **Experiments (`dream.experiment`).** The kernel skill's `await
   dream.experiment(task, rounds=4, arms=("dream", "fixed"), ...,
   llm_proposer=False, llm_dreamer=False)` and the human `/dream experiment
   [--rounds N] [--arms a,b] [--llm-proposer] [--llm-dreamer]` send a
   `dream.experiment` host request that `DreamRunService.startExperiment` runs
   in the background through `runExperimentWithAgent`
   (`src/core/dream/experiment-llm.ts`): every listed arm from the same initial
   policy, seed and per-round budget, the `fixed` arms never dreaming (the
   paper's Recursive Fixed Exploration control) and the `-guided` arms carrying
   the semantic-guidance ablation. Guided arms require `llm_proposer`; the
   host rejects them otherwise before any call, as the skill's own validation
   does. Round 1 is rolled out once and copied into every arm's store, so it is
   identical across arms by construction. The result lands at
   `<dream dir>/experiments/<experimentId>/result.json` (schema
   `prime-agent.dream.experiment/1`) for `evals/dream/plot_experiment.py`; the
   terminal durable row names that path. An experiment shares the single
   per-session run slot, the cancel relay and the `dream_run_update` stream
   with `dream.run`.

   `DreamRunStatus` gains OPTIONAL scalars for it: `kind` (`run` /
   `experiment`), `experimentId`, `arm`, `armIndex`, `armCount`, `round`,
   `rounds`, `cumulativeProbes` and `resultPath`. This is a backward-compatible
   additive change to the `dream_run_update` payload: the daemon forwards it
   opaquely and the Agents View reads only the existing fields, so there is no
   protocol version or schema revision bump. The standalone CLI's `dream
   experiment` runs the local arms only, at zero tokens; every non-local arm
   spends tokens. See `docs/dream-rsi.md` for the arms, the result schema and
   the exact headline definitions.

## Spans

`DreamRunService` opens no span of its own. `runDreamLoopWithAgent`
(`src/core/dream/llm.ts`) already mints the detached-root `dream.run` span — a
fresh trace carrying the launching turn's `trigger.trace_id` — and ends it on
success, abort (`dream.stopped = aborted`), and error, so the loop outliving the
turn is a detached root rather than a child that outlives its parent. Its
children are the ordinary rollout/dream spans (`dream.explore`, `dream.round`,
`dream.attempt`, `dream.dream`, `dream.redeploy`, and, on the LLM path,
`dream.llm_propose` / `dream.llm_dream` / `dream.llm_guidance`). `dream.run` and
`dream.redeploy` carry `dream.fixed_policy` so a control arm's trace is
recognisable without reading its records. See `docs/observability.md`.

An experiment adds one level above that. `runExperimentWithAgent` mints
`dream.experiment` as its own detached root (the same `startSpan` outside the
ambient context plus `try/finally` as `dream.run`, carrying the launching turn's
`trigger.trace_id`, ended with `dream.stopped = aborted` on cancel and
`recordError` on failure), opens one `dream.experiment_arm` child per arm
(`dream.arm`, `dream.fixed_policy`, `dream.guided`, and `dream.run_id` once the
arm's loop has an id), and runs each arm inside the experiment's context, so
every arm's `dream.run` is a detached root whose `trigger.trace_id` is the
experiment's trace. The sync local `runExperiment` behind `prime-agent dream
experiment` opens the same two spans as ordinary in-turn parents of each
`dream.run`. The guided arms' `dream.llm_guidance` is one child call per
iteration `>= 1`, with `dream.llm_fallback` marking a failed call that fell
back to empty guidance.

The per-phase `onProgress` callback threaded through `runDreamLoopWithAgent` is
observability-only: it never touches the rng, tree, scoring, or persistence, so
a run with progress reporting grows byte-identical trees to one without it, and
the synchronous local CLI path (`runDreamLoop`) is untouched.
