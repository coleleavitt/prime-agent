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
of `{current} ∪ candidates` with the current policy winning ties, so a worse or
malformed policy can never regress the deployed one. A proposed artifact is
validated by `task.deserialize` before it enters the tree and re-scored by
`task.evaluate`. Nothing evals, `Function`-constructs, or spawns any child
output. Determinism flows through one injected `SeededRng` and one injected
clock, so a recorded tree replays byte-identically.

Only the `--llm-proposer` / `--llm-dreamer` (kernel: `llm_proposer` /
`llm_dreamer`) paths spend tokens by driving a child coding agent. The default
runs the local zero-token proposer and dreamer and touches no network.

## In-session invocation surfaces

Beyond the CLI, Dream-RSI has three in-session surfaces, mirroring `/ravo`.
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

## Spans

`DreamRunService` opens no span of its own. `runDreamLoopWithAgent`
(`src/core/dream/llm.ts`) already mints the detached-root `dream.run` span — a
fresh trace carrying the launching turn's `trigger.trace_id` — and ends it on
success, abort (`dream.stopped = aborted`), and error, so the loop outliving the
turn is a detached root rather than a child that outlives its parent. Its
children are the ordinary rollout/dream spans (`dream.explore`, `dream.round`,
`dream.attempt`, `dream.dream`, `dream.redeploy`, and, on the LLM path,
`dream.llm_propose` / `dream.llm_dream`). See `docs/observability.md`.

The per-phase `onProgress` callback threaded through `runDreamLoopWithAgent` is
observability-only: it never touches the rng, tree, scoring, or persistence, so
a run with progress reporting grows byte-identical trees to one without it, and
the synchronous local CLI path (`runDreamLoop`) is untouched.
