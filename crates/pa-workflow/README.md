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
- **Workflow V2 durable store** (`v2::store`, `v2::reducer`; `WORKFLOW-V2.md` §12 slice 4,
  dormant like TS's): the owner-only per-root SQLite database at
  `<session-artifacts>/<root-session-id>/workflows/v2.sqlite` (WAL, `synchronous=FULL`,
  `foreign_keys`, 5 s busy timeout, `quick_check` + `integrity_check`, `application_id`
  `0x57325354`, `user_version` 1, the checksummed append-only migration ledger, the bound
  root-scope digest), its 15 minimum tables, and the pure reducer that folds the closed
  controller and Prime host fact vocabularies into the four-axis run/node/attempt/turn
  projections through the one §11 validator. One `BEGIN IMMEDIATE` per step: commands
  (idempotency by `requestId` and canonical request digest, revision/epoch fences, capacity
  admission with the 90% high-water mark and the free-space floor), host-inbox ingestion (dedup
  by `hostEventId`, the settlement row, and the cursor advance in one transaction), outbox claim
  (epoch-fenced lease; expiry redelivers the same canonical request) and owner-only
  acknowledgement, `RunTerminalized` equality on load with durable quarantine, terminal-text
  erasure, fenced compaction (`SNAPSHOT_REQUIRED`), and the online-backup image plus manifest.
  The on-disk format is the TS store's byte for byte (same DDL, so the TS migration checksum
  verifies; same JSON encodings in every column): a TS-written store opens and continues here,
  and the TS store opens a Rust-written one (`tests/v2_store_golden.rs`, goldens produced by
  running the TS store under node). Additions over TS that change no byte TS reads: an exclusive
  OS lock on `v2.sqlite.lock` held by the writer handle (a second writer, in-process or not,
  gets `store_writer_locked`), the writer epoch re-checked inside every write transaction
  (`store_epoch_stale` once superseded), and the §7 free-space floor enforced (TS computed it but
  its own `catch` swallowed the refusal). No production path opens a store yet.
- **Workflow V2 terminal capture and settlement** (`v2::capture`, `v2::settlement`,
  `v2::dispatch`; the pure lanes of slice 3, dormant, TS b46ec9b0e): the owned-invocation
  capture slot (one owned assistant `message_end` and one owned `agent_end` are evidence; a
  missing, duplicate, late, wrong-invocation, or wrong-binding observation is an explicit
  ambiguity, never a positional guess), exact UTF-8 result bytes and digests, the 256 KiB inline
  cap, exact usage with integral micro-USD cost; the settlement reducer that turns a capture,
  its closure, and the durable dispatch/cancel/quiescence facts into one atomic commit (the
  sealed `TurnSettlement`, the `TurnSettled` retained event the store ingests, the terminal
  receipt, the successor cursor; any unprovable effect is `execution_unknown`) and re-validates
  every digest and repeated binding of a commit; and the at-most-once dispatch guard over an
  injected journal (the physical provider call runs only after a freshly committed
  `dispatching` fact; a crash after it is terminal, never relaunched). Byte parity with the TS
  slot and reducer is golden-tested (`tests/v2_settlement.rs`, 30 cases generated by running the
  TS modules under node), and every TS-settled event ingests into the slice-4 store.

## Non-goals

- No session: the turn never touches the session's stream, retry driver, request timing,
  semantic-edge recorder, messages, or files. Provider I/O is its only effect.
- No tools, no multi-turn runs (`maxTurns` is exactly 1 on the V1 wire).
- No Workflow V2 controller: the store holds no policy and nothing calls it. Missing are slice
  3's native halves — the host request-id composite admission in the daemon's RLM spawn ledger
  (`pa-daemon` `rlm_ledger`, TS `RlmCompositeAdmissionLedger`), the per-worker retained journal
  behind the dispatch guard, and the tools-none retained child runtime that feeds the capture
  slot from a real child session (TS `workflow-v2-retained-profile.ts`) — then the controller/executor
  adapter and the create/start path (slice 5), settlement ingestion policy, acceptance, budgets,
  retry, and terminalization (slice 6), cancellation, quiescence, tombstone-first delete, and the
  crash-window suite (slice 7), the Python/status/events/view projections (slice 8), and the
  release attestation that would enable the capability (slice 9), plus the TS daemon OS fence
  and supervisor control DB. The TS fork built slices 2-4 dormant and never a controller.

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
- `v2::store` — `Store` (`open`, `acquire_writer`, `validate`, `apply_command`,
  `ingest_host_event`, `claim_outbox`, `acknowledge_outbox`, `run_projection`, `load_aggregate`,
  `command_receipt`, `list_events`, `operation`, `host_cursor`, `table_count`,
  `reconcile_terminal_mismatch`, `erase_run_text`, `compact_events`, `backup_to`),
  `probe_capability`, `StoreOptions`, `CommandEffect`, `StoreError`/`StoreCode` (the TS codes).
- `v2::capture` — `TerminalCaptureSlot`, `classify_result`, `normalize_usage`,
  `decimal_to_microusd`, `bound_text`, `seal`/`sealed_digest`, the capture/closure types.
- `v2::settlement` — `reduce_settlement`, `validate_settlement_commit`, `SettlementInput`,
  `AtomicSettlementCommit`.
- `v2::dispatch` — `RetainedDispatchGuard::dispatch_once` over a `DispatchJournal`.
- `v2::reducer` — `reduce_fact`, `reduce_run`, `revalidate_aggregate`, `projection`,
  `check_terminalized_outcome`, `RunAggregate`, `ReducerError`/`ReducerCode` (the TS codes).

## Seams

- `pa_core::features::SessionFeature::register_host_handlers` with `SessionFeatureContext`
  (agent dir, cwd, session model, `FeatureTelemetry`).
- `pa_core::kernel::shared::host_request_cancellation` (the `host_cancel` token).
- Native machinery reused, not reimplemented: `rlm_in_process::resolve_child_model` (the RLM
  child selector resolution), `ModelRegistry` (catalog, auth, `provider_request_config`), and
  `provider_adapter::stream_once` (the per-request provider transport).
- The store's SQLite is `rusqlite` with bundled SQLite, a dependency of this crate only (the
  native `--no-default-features` build carries none); `pa_core::platform::perms` for the
  owner-only modes and ownership checks; `pa_core::session::manager::format_iso` for the
  `toISOString` timestamps; std's `File::try_lock` for the writer lock.

## Files owned

None written today. V1 is ephemeral and V2's served action (`validate`) is pure. The dormant
store, once a controller opens it, owns `<session-artifacts>/<root-session-id>/workflows/`:
`v2.sqlite` (with its `-wal` / `-shm`), `v2.sqlite.lock`, and any backup image plus
`<image>.manifest.json` it is asked to write — all owner-only (0700 dir, 0600 files).

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
- The dormant store, reducer, capture, settlement, and dispatch guard emit nothing (no product
  path reaches them).
