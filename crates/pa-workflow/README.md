# pa-workflow

The fork's Workflow host: the kernel host requests the runtime's `rlm.workflow` modules send.
A feature crate under `docs/fork-feature-crates.md`; `pa-cli` installs it behind its `workflow`
Cargo feature (on by default). Behavioural spec: the TS host on `perf/session-catalog-resume`
(`workflow-v1-wire.ts`, `run-workflow-agent.ts`, the `"workflow.run_agent"` handler in
`agent-session.ts`, `docs/WORKFLOW-V1-HOST-ACCEPTANCE.md`, `docs/WORKFLOW-V1-HOST-FINAL-REVIEW.md`).

## Scope

- **Workflow V1** (`v1`): the single `workflow.run_agent` operation. The runtime
  (`prime-agent-runtime/src/rlm/workflow.py`) sends a closed `prime.workflow.run-agent/v1`
  request; the host decodes it, resolves the model (`null` = the session model; a selector
  resolves exactly like an RLM child's), fails closed before any provider I/O unless the model
  is registered and its provider holds an API key, a configured credential-bearing header, or
  an explicit `authHeader: false` policy, runs **one** tool-less provider turn (no system
  prompt, no retry, no session id, no parent headers), and settles with the closed
  `prime.workflow.run-agent-result/v1` reply (result text joined without separators, its UTF-8
  size and SHA-256, host-observed usage, the soft-budget verdict).
- Cancellation: a runtime `host_cancel` fires the request's cancellation token; the host closes
  the provider stream and reports `cancelled`, or `execution_unknown`/`drain_timeout` when the
  turn does not settle within `drainTimeoutMs`.
- V2 (the durable workflow host, Phase 5) lands in this crate beside `v1`, under `v2`.

## Non-goals

- No session: the turn never touches the session's stream, retry driver, request timing,
  semantic-edge recorder, messages, or files. Provider I/O is its only effect.
- No tools, no multi-turn runs (`maxTurns` is exactly 1 on the V1 wire).
- No Workflow V2 store, OS fence, or supervisor control DB (Phase 5).

## Public API

- `WorkflowFeature` — the `pa_core::features::SessionFeature` `pa-cli` installs.
- `v1::register_host_handlers`, `v1::RUN_AGENT_REQUEST_TYPE`.
- `v1::wire` — the request decoder and reply builder (`decode_request`, `reply`, and the wire
  types), shared with tests and, later, V2.

## Seams

- `pa_core::features::SessionFeature::register_host_handlers` with `SessionFeatureContext`
  (agent dir, cwd, session model, `FeatureTelemetry`).
- `pa_core::kernel::shared::host_request_cancellation` (the `host_cancel` token).
- Native machinery reused, not reimplemented: `rlm_in_process::resolve_child_model` (the RLM
  child selector resolution), `ModelRegistry` (catalog, auth, `provider_request_config`), and
  `provider_adapter::stream_once` (the per-request provider transport).

## Files owned

None. V1 is ephemeral: it writes nothing under `~/.prime/agent/` or the session artifacts.

## Telemetry

- `workflow_run_agent` (`pa-telemetry` catalog v4), once per settled request: `outcome`,
  `stop_reason`, `turns_started`, `duration_ms`, `total_tokens`, `budget_exhausted`. Never the
  prompt, result, request/node ids, or model ids.
- `tracing`: the `workflow.run_agent` span and a `pa_workflow` "workflow.run_agent settled"
  event with the same classification.
