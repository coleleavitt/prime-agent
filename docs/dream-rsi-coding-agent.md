# Dream-RSI

Dream-RSI (Zheng et al., 2026) grows a discovery tree over a scored task,
freezes each tree into a zero-cost replay simulator, and improves a typed,
serializable exploration policy: it rolls out with the default policy, dreams
one or more candidate policies that should score no worse on replay, selects a
no-regression policy, and redeploys it in a fresh rollout. The framework is the
`pa-dream` feature crate (`crates/pa-dream/`); the standalone `prime-agent
dream` CLI (`command.rs`, wired by `crates/pa-cli/src/dream_command.rs`)
drives the synchronous, local, zero-token path. `docs/dream-rsi.md` is the full
reference; this page covers soundness and the in-session surfaces.

## Soundness

A dreamed exploration policy is **data**, never code. The LLM dreamer emits a
flat JSON policy that `parse_exploration_policy` accepts only if every
field is a known key of the right type and in range;
`select_best_policy` returns the argmax of the current policy plus the
candidates over the candidates whose replay quality on every measured tree is
no lower than the current policy's, with the current policy winning ties, so a
worse, malformed or exploration-collapsing policy can never regress the
deployed one on replay, and every adoption is then checked online by a
probation rollout that reverts it below the incumbent's floor. The replay
objective is scale-invariant (quality normalized to the pool's score range,
cost as a fraction of the `W * k1` probe budget, a stop-early credit only where
the candidate's other replays vouch for it; `docs/dream-rsi.md` has the
derivation and the recorded regressions that forced it). A proposed artifact is
validated by the task's deserializer before it enters the tree and re-scored by
the task's `evaluate`. Nothing evaluates or spawns any child output except the
`python-speedup` task's bounded, isolated `python3 -I -B` run of a candidate
program. Determinism flows through one injected `SeededRng` and one injected
clock: every rng fork is labelled by seed, iteration, round, parent seq and
child slot, never by an id, so a seed fixes every score and shape while the
clock reaches only the on-disk ids, and a recorded tree replays
byte-identically. The local path writes the same bytes as the fork's TS product
(`crates/pa-dream/tests/golden.rs`).

Only the `--llm-proposer` / `--llm-dreamer` (kernel: `llm_proposer` /
`llm_dreamer`) paths spend tokens by driving a tool-less child agent
(`AgentRunAgent`). The default runs the local zero-token proposer
and dreamer and touches no network.

## In-session invocation surfaces

`pa-cli` installs `DreamFeature` behind its `dream` Cargo feature
(`crates/pa-cli/src/features.rs`). It has four in-session surfaces, mirroring
`/ravo`. They are available only in a top-level session (RLM depth 0) with a
session artifact directory (`dream_allowed`), so the surfaces stay in
lockstep with where the `dream.*` host handlers are registered: other sessions
get no handlers (the kernel's `dream.*` calls fail as unregistered host
requests), do not see the skill, and get `Dream-RSI is not available in this
session` from `/dream`. Each allowed session owns one
`DreamRunService` (one run slot), created on first use.

1. **Model (kernel skill).** The bundled `dream` skill
   (`skills/.features/dream/`, loaded only while the feature is installed)
   exposes `await dream.run(task=..., iterations=..., llm_proposer=False,
   llm_dreamer=False, ...)`, `await dream.status()`, and `await
   dream.cancel()`. Each call is a thin wrapper over `rlm.host_request` sending
   a `dream.run` / `dream.status` / `dream.cancel` host request, which the
   feature registers through `SessionFeature::register_host_handlers`
   (`DREAM_REQUEST_TYPES`). `dream.run` returns immediately with `started`,
   `runId` and a note; the run continues in the background.

2. **Human (`/dream`).** `/dream [experiment] [--task <id>] [--n N] [--seed N]
   [--seeds a,b,c] [--iterations N] [--rounds N] [--arms dream,fixed]
   [--workers N] [--k1 N] [--k2 N] [--dreams N] [--priming none|diverse]
   [--model provider/id] [--thinking <level>] [--max-output-tokens N]
   [--llm-proposer] [--llm-dreamer]` is a session slash command
   (`SessionFeature::slash_commands`, parsed by `parse_dream_command`).
   Its result row reads `Dream-RSI run <id> started: <task>`, and when the
   background run settles a durable terminal row follows (`Dream-RSI run <id>
   completed`, `cancelled`, or a failure row).

3. **Live progress (feature status).** `DreamRunService` publishes a
   `DreamRunStatus` snapshot through its `on_update` sink at each phase
   boundary; the feature hands it to `pa_core::features::publish_feature_status`
   with the agents-view line from `dream_status_line`. The daemon worker puts it
   on the session's roster summary (`featureStatus`, key `dream`) and sends a
   `feature_status` session event, so a run's phase, iteration, best node score,
   and final policy score appear live in the agents view. The status keeps the
   TS keys (`runId`, `phase`, `bestNodeScore`, ...).

4. **Experiments (`dream.experiment`).** The kernel skill's `await
   dream.experiment(task, rounds=4, arms=("dream", "fixed"), ...,
   llm_proposer=False, llm_dreamer=False)` and the human `/dream experiment
   [--rounds N] [--arms a,b] [--llm-proposer] [--llm-dreamer]` send a
   `dream.experiment` host request that `start_experiment` on `DreamRunService`
   runs in the background: every listed arm from the same initial policy, seed
   and per-round budget, the `fixed` arms never dreaming (the paper's Recursive
   Fixed Exploration control) and the `-guided` arms carrying the
   semantic-guidance ablation. With either LLM toggle on the arms run through
   `AgentArmRunner`, otherwise through the same local runner as
   the CLI. Guided arms require `llm_proposer`; the host rejects them otherwise
   before any call, as the skill's own validation does. On the LLM path round 1
   is rolled out once and copied into every arm's store, so it is identical
   across arms by construction (the local path rolls it out per arm, byte for
   byte the same). The result lands at
   `<dream dir>/experiments/<experimentId>/result.json` (schema
   `prime-agent.dream.experiment/1`) for `evals/dream/plot_experiment.py`; the
   terminal durable row names that path. An experiment shares the single
   per-session run slot, the cancel relay and the status stream with
   `dream.run`.

   `DreamRunStatus` carries optional scalars for it: `kind` (`run` /
   `experiment`), `experimentId`, `arm`, `armIndex`, `armCount`, `round`,
   `rounds`, `cumulativeProbes` and `resultPath`, plus `seed`, `seedIndex`,
   `seedCount`, `resultPaths` and `tokens` for a multi-seed experiment. They
   are additive and the daemon forwards the status opaquely, so no wire
   protocol version changes. The standalone CLI's `dream experiment` runs the
   local arms only, at zero tokens; every non-local arm spends tokens. See
   `docs/dream-rsi.md` for the arms, the result schema and the exact headline
   definitions.

Each in-session run or experiment that ends emits one `dream_session_run`
telemetry event (`kind`, `surface`, `task`, `outcome`, `llm_proposer`,
`llm_dreamer`, `seeds`, `improved`, `duration_ms`; counts and vocabularies
only). At process exit the feature's `flush` cancels every run at its next
boundary.

## Spans

`DreamRunService` opens no span of its own. `run_dream_loop_with_agent`
opens the detached-root `dream.run` span (`parent: None`) — a fresh trace
carrying the launching turn's `trigger.trace_id`, which
`start` on `DreamRunService` reads on the caller's thread — and records
`dream.stopped = aborted` on a cancel and `error` on a failure, so the loop
outliving the turn is a detached root rather than a child that outlives its
parent. Its children are the ordinary rollout/dream spans (`dream.explore`,
`dream.round`, `dream.attempt`, `dream.dream`, `dream.redeploy`, and, on the
LLM path, `dream.llm_propose` / `dream.llm_dream` / `dream.llm_guidance`).
`dream.run` and `dream.redeploy` carry `dream.fixed_policy` so a control arm's
trace is recognisable without reading its records. The spans are `tracing`
spans; `pa-trace` records them in `agent.jsonl`. See `docs/observability.md`.

An experiment adds one level above that. `run_experiment_with_hooks`
(with `ExperimentHooks` field `detached` set, as `DreamRunService` does) opens
`dream.experiment` as its own detached root carrying the launching turn's
`trigger.trace_id`, records `dream.stopped = aborted` on cancel and `error` on
failure, opens one `dream.experiment_arm` child per arm (`dream.arm`,
`dream.fixed_policy`, `dream.guided`, and `dream.run_id` once the arm's loop
has an id), and runs each arm inside the experiment's span. On the LLM path
every arm's `dream.run` is a detached root whose `trigger.trace_id` is the
experiment's trace; with the local runner (the CLI, or in-session with both
toggles off) each arm's `dream.run` is an ordinary child of its arm span. The
guided arms' `dream.llm_guidance` is one child call per iteration `>= 1`, with
`dream.llm_fallback` marking a failed call that fell back to empty guidance.

The per-phase `on_progress` callback threaded through
`run_dream_loop_with_agent` is observability-only: it never touches the rng,
tree, scoring, or persistence, so a run with progress reporting grows
byte-identical trees to one without it, and the synchronous local CLI path
(`run_dream_loop`) is untouched.

## Differences from the TS fork

- Progress is a generic `feature_status` event and roster entry, not the TS
  `dream_run_update` event behind a `dream_run_updates` capability.
- Dream children open no `rlm.run_agent` span; their model, thinking level and
  cap are on `dream.run` (`dream.child_model`, `dream.child_thinking`,
  `dream.child_max_output_tokens`).
- A child request without a thinking level runs with thinking off rather than
  inheriting the parent's level; the run service always sets one (`off` by
  default), so this shows only to a direct caller of the `RunAgent` seam.

`docs/dream-rsi.md` → "Differences from the TS fork" has the full list.
