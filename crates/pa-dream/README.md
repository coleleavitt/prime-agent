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

## Non-goals (this slice)

Anything that spends a token: the LLM proposer and dreamer, the semantic
guidance writer, the in-session run service (`dream.run` / `dream.status` /
`dream.cancel` / `dream.experiment` host requests, `/dream`,
`dream_run_update` pushes) and the `dream` kernel skill. The CLI rejects
`--llm-proposer`, `--llm-dreamer` and the guided arms with exit 2, as the TS
standalone CLI does.

## Public API and seams

- `command::run_dream_command(args, io) -> DreamCommandOutcome` (with a
  `DreamRunReport` for adoption telemetry) and the help constants
  `DREAM_USAGE`, `DREAM_SUMMARY`, `DREAM_DESCRIPTION`, `DREAM_OPTIONS`,
  `DREAM_EXAMPLES`. `pa-cli` dispatches `prime-agent dream` here behind its
  `dream` Cargo feature and emits the telemetry.
- The extension seams for the in-session phase: `proposer::Proposer` (a
  generation attempt; the LLM proposer stamps `origin: llm` and keeps a
  `ProposalTally`), `improve::CandidateSource` (a dreaming step's candidate
  set; the LLM dreamer), `experiment::ExperimentArmRunner` (how an arm's loop
  is driven; the LLM runner shares round 1), and `task::ScoredTask` /
  `task::DynTask` for new tasks. The loop, selection and records already
  carry the provenance fields those paths fill (`origin`, `proposals`,
  `handlerCalls`, `tokens`, `dreamer`).
- Native seams used: none beyond `pa-cli`'s command registry. Spans are
  `tracing` spans named as the TS spans (`dream.run`, `dream.explore`,
  `dream.round`, `dream.attempt`, `dream.dream`, `dream.replay`,
  `dream.candidate`, `dream.redeploy`, `dream.experiment`,
  `dream.experiment_arm`) with the TS attribute names as fields.

## Files owned

Under the dream dir (`$PRIME_AGENT_DREAM_DIR`, tilde-expanded, else
`<agent dir>/dream`), byte-compatible with the TS product:

- `trees/<treeId>.jsonl`, `trees/<treeId>/blobs/<seq>.json`
- `dreams/<runId>.jsonl`
- `experiments/<experimentId>/<arm>/…` (a complete store per arm) and
  `experiments/<experimentId>/result.json`

Directories are created 0700 and files 0600. Nothing else is written; no
`harness_state.json` key is used.

## Telemetry

`dream_run` (pa-telemetry catalog, schema v4): one per parsed `prime-agent
dream` invocation, emitted by `pa-cli` — `subcommand`, `task`, `outcome`
(`completed` / `failed` / `unavailable`), `rollouts`, `probes`, `improved`,
`duration_ms`. Counts and fixed vocabularies only.

## Dependencies

`pa-types` (agent dir), `serde`, `serde_json`, `sha2`, `thiserror`,
`tracing` — all already in the workspace graph.
