# pa-workflow

The fork's Workflow host: the kernel host requests the runtime's `rlm.workflow` modules send.
A feature crate under `docs/fork-feature-crates.md`; `pa-cli` installs it behind its `workflow`
Cargo feature (on by default). Behavioural spec: the TS host on `perf/session-catalog-resume`
(`workflow-v1-wire.ts`, `run-workflow-agent.ts`, the `"workflow.run_agent"` handler in
`agent-session.ts`, `docs/WORKFLOW-V1-HOST-ACCEPTANCE.md`, `docs/WORKFLOW-V1-HOST-FINAL-REVIEW.md`;
for V2 `workflow-v2-wire.ts`, `workflow-v2-capability.ts`, the reducer's projection validator, and
the normative `WORKFLOW-V2.md` + `workflow-v2.schema.json` of the pinned pi-plugin-workflow
authority bundle `af31f5e`).

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
- **Workflow V2** (`v2`): the closed `workflow.v2.request` envelope the runtime
  (`prime-agent-runtime/src/rlm/workflow_v2.py`) sends. The host strictly decodes the
  `prime.workflow.request/v2` family against the runtime's packaged schema (embedded byte for
  byte, digest-pinned). `validate` is served in full and is pure: structural validation, the
  definition semantics (unique ids, known outputs/dependencies, no self edge, acyclic, total
  budget covers each node), and the RFC 8785 `definitionDigest`; an invalid definition answers
  `valid: false` with the bounded error. `create`, `start`, `cancel`, `retry`, `status`, and
  `events` answer the closed `CAPABILITY_UNAVAILABLE` public error — never an empty run or page
  — because the durable controller they need does not exist (the TS host shipped the whole
  capability disabled). An envelope outside the family answers `INVALID_REQUEST` (a `create`
  with an invalid definition, `INVALID_DEFINITION`); one without a valid `requestId` fails the
  host request, which the runtime reports as `CapabilityUnavailable`.
- The V2 wire is complete for every protocol in the family (public requests/results/errors,
  the eight retained `child.*` operations, controller and retained events, capability, view,
  settlements), with the strict JSON bounds (duplicate keys, 32 levels, 10,000 nodes, 1 MiB),
  result byte/digest bindings, the one §11 projection validator, and the exact retained-host
  capability vector (negotiation stays unavailable).

## Non-goals

- No session: the turn never touches the session's stream, retry driver, request timing,
  semantic-edge recorder, messages, or files. Provider I/O is its only effect.
- No tools, no multi-turn runs (`maxTurns` is exactly 1 on the V1 wire).
- No Workflow V2 durable controller: no SQLite store (`<session-artifacts>/<root>/workflows/
  v2.sqlite`), reducer/outbox/inbox, scheduler, acceptance/budget policy, retained-child host
  (`child.*`), settlement capture, cancellation/deletion/recovery, daemon OS fence, or supervisor
  control DB. Those are `WORKFLOW-V2.md` §12 slices 3-9; the TS fork built slices 2-4 dormant and
  never a controller.

## Public API

- `WorkflowFeature` — the `pa_core::features::SessionFeature` `pa-cli` installs.
- `v1::register_host_handlers`, `v1::RUN_AGENT_REQUEST_TYPE`.
- `v1::wire` — the request decoder and reply builder (`decode_request`, `reply`, and the wire
  types).
- `v2::register_host_handlers`, `v2::REQUEST_TYPE`, `v2::host::answer` (the pure request →
  reply mapping).
- `v2::wire` (`decode_public_request`, `decode_definition`, `decode_public_result`,
  `decode_retained_request`, `decode_as`, `canonical_json`, `request_digest`), `v2::json`
  (`parse`, `check_bounds`), `v2::schema`, `v2::projection`
  (`validate_projection_semantics`), `v2::capability`.

## Seams

- `pa_core::features::SessionFeature::register_host_handlers` with `SessionFeatureContext`
  (agent dir, cwd, session model, `FeatureTelemetry`).
- `pa_core::kernel::shared::host_request_cancellation` (the `host_cancel` token).
- Native machinery reused, not reimplemented: `rlm_in_process::resolve_child_model` (the RLM
  child selector resolution), `ModelRegistry` (catalog, auth, `provider_request_config`), and
  `provider_adapter::stream_once` (the per-request provider transport).

## Files owned

None. V1 is ephemeral and V2's served action (`validate`) is pure: neither writes anything under
`~/.prime/agent/` or the session artifacts.

## Telemetry

- `workflow_run_agent` (`pa-telemetry` catalog v4), once per settled request: `outcome`,
  `stop_reason`, `turns_started`, `duration_ms`, `total_tokens`, `budget_exhausted`. Never the
  prompt, result, request/node ids, or model ids.
- `workflow_durable_request` (`pa-telemetry` catalog v4), once per answered request: `action`
  (`unknown` when the envelope named none) and `outcome` (`valid`, `invalid_definition`,
  `invalid_request`, `capability_unavailable`). Never the definition, prompts, models, or ids.
- `tracing`: the `workflow.run_agent` span and a `pa_workflow` "workflow.run_agent settled"
  event with the same classification; the `workflow.v2.request` span and a "workflow.v2.request
  answered" event with the V2 action and outcome.
