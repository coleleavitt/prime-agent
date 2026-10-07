# pa-dream

Dream-RSI (Zheng et al., 2026), ported from the fork's TS product
(`packages/coding-agent/src/core/dream/` and `src/cli/dream-command.ts` on
`perf/session-catalog-resume`; behavioural spec `docs/dream-rsi.md`). A
feature crate under `docs/fork-feature-crates.md`.

## Scope

The standalone, zero-token runner behind `prime-agent dream`:

- **Discovery trees** (`tree`, `records`, `store`): JSONL trees under
  `<dream dir>/trees/<treeId>.jsonl` with canonical-JSON artifact blobs under
  `<treeId>/blobs/<seq>.json`.
- **Online exploration** (`rollout`, `interpreter`, `proposer`): a fixed
  interpreter of a typed, serializable exploration policy (`policy`) grows a
  tree over a scored task (`tasks`: circle-packing, sum-difference,
  autocorrelation, python-speedup).
- **Frozen replay** (`replay`) and the **objective V** (`objective`): zero-cost
  re-walks of a recorded tree with out-of-support accounting.
- **Dreaming** (`improve`): the local dreamer, the no-regression selection over
  the measured pool with evidence-backed spend, per-candidate verdicts, the
  lever scan.
- **The loop** (`dream_loop`): explore → dream → redeploy, the probation that
  reverts an adoption, the fixed-exploration control, pool priming, and the
  per-run **dreams log** (`dreams`).
- **Experiments** (`experiment`): the dream/fixed arms, per-arm stores, the
  headline multipliers and `result.json` (schema
  `prime-agent.dream.experiment/1`, read by `evals/dream/plot_experiment.py`).
- **The CLI** (`command`): `prime-agent dream
  [rollout|replay|improve|loop|experiment|status|show]`, argv parsing and the
  text/JSON output.

Determinism is the contract: one seed and one clock give byte-identical
trees, blobs, dreams logs and `result.json` to the TS product. `json` writes
numbers as `JSON.stringify` does and parses decimals exactly; `js_math` is
V8's own `Math.log`/`Math.cos` (fdlibm), so seeded gaussians are bit-identical;
`collate` sorts ids like `localeCompare`. `tests/golden.rs` checks all of it
against goldens produced by the TS code on Node.

## In-session Dream (the token-spending half)

- **The child seam** (`child`): `RunAgent` runs one tool-less child agent
  to a terminal result; `run_structured_child` classifies it (status,
  `extract_json_value` parse, shape, `length` at the output cap).
  `agent_runner::AgentRunAgent` is the session's implementation: the native
  agent loop with no tools on the provider transport, the model resolved
  like an RLM child's (the session model by default) and its credential
  cleared first, the thinking level checked, `max_tokens` = cap + thinking
  allowance (never above the model's limit), turn and token limits on
  continuing turns, cancellation.
- **LLM proposer, dreamer, guidance writer** (`llm`): the TS `llm.ts`
  prompts byte for byte, one retry on a retryable rejection, the local
  mutator standing in (`origin: local`), the dreamer's strict per-entry
  parse with drop reasons and local top-up on `dream-fallback:<i>`, the
  dreamer's pool digest and verdict history (revocations included), the
  guidance digest; every rejection in `rejections/<runKey>.jsonl`.
- **The agent loop** (`llm_loop::run_dream_loop_with_agent`): a detached
  `dream.run` root with `trigger.trace_id`, independent toggles, per-round
  calls/tokens per role and the proposer tally, shared round 1, priming,
  probation, progress events, cancellation. Both toggles off is the local
  loop byte for byte.
- **Experiments** (`experiment::run_experiment_with_hooks`,
  `experiment_llm::AgentArmRunner`): the four arms, round 1 rolled out once
  and `copy_tree`d into every arm, the arm mode with the child model,
  thinking and cap, cancellation, progress, a detached `dream.experiment`.
- **The run service** (`run_service::DreamRunService`): one slot per
  session, `DreamRunStatus` snapshots, multi-seed experiments,
  `dream_child_scope` (thinking off, 8 turns, per-role caps through
  `RoleCappedRunner`).
- **The session feature** (`session::DreamFeature`, installed by `pa-cli`):
  the `dream.run` / `dream.status` / `dream.cancel` / `dream.experiment`
  host requests (TS replies and messages, `requests`), `/dream` (its result
  row and a durable terminal row when the run ends), the bundled `dream`
  kernel skill (`skills/.features/dream`), status pushes with the TS
  agents-view line, and the adoption event. Offered only to a top-level
  session with a session artifact directory (`dream_allowed`; TS
  `_autoRefineAllowedForSession`). The `dream` skill follows the same rule:
  `session_skill_visible` hides it from any other session (TS
  `_modelVisibleSkills`), so no session lists a skill whose calls would fail.

## Non-goals and known differences

- The service runs on its own thread; status key order is the struct's,
  not the TS spread order (the reply is not persisted).
- A rejection excerpt or guidance artifact cut through a surrogate pair
  holds U+FFFD where JS keeps the lone surrogate.

## Public API and seams

- `command::run_dream_command` (the CLI, unchanged) and the in-session
  modules above.
- Seams used: `pa-cli`'s command registry; `SessionFeature::
  register_host_handlers`, `slash_commands` / `execute_slash_command`,
  `bundled_skills`, `session_skill_visible`, `flush`; `pa_core::features::publish_feature_status`
  (the daemon's `feature_status` event and roster `featureStatus`, the
  agents-view line). Spans are `tracing` spans named as the TS spans,
  plus `dream.llm_propose`, `dream.llm_dream`, `dream.llm_guidance`.
- The crate's own extension seams: `proposer::Proposer` (fallible, so a
  cancel stops a rollout), `improve::CandidateSource`,
  `experiment::ExperimentArmRunner` (`prepare` shares round 1),
  `child::RunAgent`.

## Files owned

Under the dream dir (`$PRIME_AGENT_DREAM_DIR`, tilde-expanded, else
`<agent dir>/dream`), byte-compatible with the TS product:

- `trees/<treeId>.jsonl`, `trees/<treeId>/blobs/<seq>.json`
- `dreams/<runId>.jsonl`
- `experiments/<experimentId>/<arm>/…` (a complete store per arm) and
  `experiments/<experimentId>/result.json`
- `rejections/<runKey>.jsonl` (the LLM path only; an experiment's shared
  round logs `<experimentId>-shared` under its first arm's store)

Directories are created 0700 and files 0600. Nothing else is written; no
`harness_state.json` key is used.

## Telemetry

`dream_run` (pa-telemetry catalog, schema v4): one per parsed `prime-agent
dream` invocation, emitted by `pa-cli` — `subcommand`, `task`, `outcome`
(`completed` / `failed` / `unavailable`), `rollouts`, `probes`, `improved`,
`duration_ms`. Counts and fixed vocabularies only.

`dream_session_run` (schema v4): one per in-session run or experiment
reaching its end — `kind` (`run`/`experiment`), `surface`
(`skill`/`command`), `task`, `outcome` (`completed`/`cancelled`/`failed`),
`llm_proposer`, `llm_dreamer`, `seeds`, `improved`, `duration_ms`.

## Dependencies

`pa-types`, `pa-agent`, `pa-core`, `pa-telemetry`, `anyhow`, `serde`,
`serde_json`, `sha2`, `thiserror`, `tokio`, `tokio-util`, `tracing`, `uuid`
— all already in the workspace graph.
