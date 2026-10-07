# Observability: end-to-end trace context

Status: implemented in the Rust host by the `pa-trace` feature crate
(`crates/pa-trace`, wired in `pa-cli` behind `feature = "trace"`), plus the
kernel runtime's `rlm.trace` (`prime-agent-runtime/src/rlm/trace.py`). This
document describes that implementation. The TypeScript fork it was ported
from is the behavioural spec; what the port does not have is listed under
[Differences from the TS fork](#differences-from-the-ts-fork).

## Goal

A user turn can be followed across processes with one **W3C `traceparent`**:
the turn loop, each provider request, each tool execution, the Python kernel
cell that ran, the host requests it made and the `bash()` commands it
started. No OpenTelemetry dependency is required; the format and vocabulary
mirror OpenTelemetry, and an optional dependency-free OTLP/HTTP JSON exporter
reads the same spans.

Native crates never mint ids or write the log. They emit plain `tracing`
spans and events; `pa-trace` is the subscriber that turns them into
`~/.prime/agent/logs/agent.jsonl` records. Built without the feature
(`pa-cli --no-default-features`), nothing is recorded, no carrier is set and
no trace file is opened.

## Identity

`traceparent = "00-" + traceId(32 hex) + "-" + spanId(16 hex) + "-" + flags(2 hex)`

* The value type and its strict parse/format are `pa_types::trace_context`
  (`TraceContext::parse`: version `00`, lowercase hex of the exact lengths,
  non-zero ids; anything else is `None`). The Python side is
  `parse_traceparent` / `format_traceparent` in `rlm.trace`.
* Every recorded span gets a child context of, in order: a `traceparent`
  field on the span itself (a remote parent), its parent span, or the
  inbound `TRACEPARENT` this process was started with. With none of those it
  starts a new trace.
* `flags` is `01` (sampled) on every locally minted context.

## How it is wired

* `pa_cli::features::install_enabled_features` calls `pa_trace::install`
  once, before any session or worker starts, with
  `RecorderConfig::from_env` (`TRACEPARENT`, `OTEL_EXPORTER_OTLP_ENDPOINT`,
  `OTEL_EXPORTER_OTLP_HEADERS`). It does no I/O: the log file and the writer
  thread start with the first record.
* `pa_trace::install` sets the recorder as the global subscriber and
  registers it as the context source
  (`pa_types::trace_context::set_current_context_source`), so
  `pa_types::trace_context::current()` and `current_traceparent()` answer the
  active span for native carriers.
* Only callsites with a `pa_*` target at INFO or above are recorded, plus the
  two special targets below. Everything else (other crates, DEBUG and TRACE
  spans such as `mcp.session.open`, `mcp.session.call`,
  `tool_bash_execute`) is disabled at the callsite.
* Every process that runs `prime-agent` installs its own recorder: the
  client, the daemon supervisor and each session worker append to the same
  file.

## Span names (stable, dotted)

The last column names the file that opens the span, so a grep for the name
lands on it. Native spans use `tracing::instrument` or
`tracing::info_span!`; runtime spans are opened in Python and forwarded by
the host (see [Kernel runtime spans](#kernel-runtime-spans)).

Native crates:

| span | attributes | opened in |
|---|---|---|
| `agent.turn` | `session.id`, `turn.index`, `llm.provider`, `llm.model`, `turn.stop_reason`, `turn.aborted`, `turn.tool_calls`, `turn.tool_errors`, `turn.tool_error_names`, `error` | `crates/pa-agent/src/agent_loop/run.rs` (`run_turn`) |
| `llm.request` | `llm.provider`, `llm.api`, `llm.model`, `llm.base_url`, `llm.stop_reason`, `llm.usage.input`, `llm.usage.output`, `error` | `crates/pa-agent/src/agent_loop/response.rs` (`stream_assistant_attempt`, one per attempt) |
| `tool.execute` | `tool.name`, `tool.call_id`, `tool.aborted`, `error`; `failure.fingerprint` (added by `pa-ledger`) | `crates/pa-agent/src/agent_loop/tools.rs` (`complete_tool_call`) |
| `kernel.start` | `kernel.python` (`venv` or the override path), `kernel.restore`, `kernel.pid` | `crates/pa-core/src/kernel/manager/startup.rs` (`do_start`; the kernel's `TRACEPARENT` names this span) |
| `kernel.execute` | `kernel.request_id`, `kernel.request_type`, `kernel.status`, `error` | `crates/pa-core/src/kernel/manager/requests.rs` (`execute_inner`; the request frame's `traceparent` names this span, so the runtime's `kernel.cell` nests under it) |
| `session.load` | `session.path`, `session.entries` | `crates/pa-core/src/session/manager/lifecycle.rs` (`open`) |
| `session.compact` | `llm.provider`, `llm.model`, `compact.skipped` | `crates/pa-core/src/session_engine/compaction_arms.rs` (`compact`) |

Feature crates (each present only when its feature is built in):

| span | attributes | opened in |
|---|---|---|
| `harness.ledger.flush` | `session.id`, `ledger.scope` (`global`), `ledger.observations`, `ledger.verifications`, `ledger.fingerprints`, `error`; from `pa-ravo`: `ledger.regressions`, `trust.recurrences`, `trust.adjudications`, `trust.faulted`, `trust.clean`, `trust.contested` | `crates/pa-ledger/src/feature.rs` (around each global flush only, under the harness state lock; `PRIME_AGENT_GLOBAL_LEDGER=0` keeps the ledger per session and opens no span) |
| `harness.trust.adjudicate` | `session.id`, `trust.jobs`, `trust.windows`, `trust.ran`, `trust.upheld`, `trust.cleared`, `trust.unverifiable`, `trust.skipped`, `trust.aborted` | `crates/pa-ravo/src/trust_adjudication.rs` (a detached root: the post-commit trust replays, run off the turn path) |
| `ravo.run` | `ravo.run_id`, `ravo.resumed`, `ravo.reason`, `ravo.rounds`, `ravo.repairs`, `ravo.spent_tokens`, `ravo.certificate_digest` | `crates/pa-ravo/src/run/controller.rs` (one per `ravo.run` host request or `/ravo`) |
| `ravo.round` | `ravo.round`, `ravo.phase`, `ravo.outcome`, `ravo.reason` | `crates/pa-ravo/src/run/controller.rs` |
| `ravo.proposal` | `ravo.round`, `ravo.kind`, `ravo.proposal_id`, `ravo.candidate_tokens` | `crates/pa-ravo/src/run/controller.rs` (the implement or repair child call) |
| `ravo.evaluation` | `ravo.proposal_id`, `ravo.evaluator`, `ravo.evaluator_kind`, `ravo.verdict` | `crates/pa-ravo/src/run/controller.rs` (each evaluator and the commit gate) |
| `recall.mark` | `recall.repo_key`, `recall.dirty_count`, `recall.claims`, `recall.unverifiable`, `recall.ms`, `recall.skipped`, `recall.skip_reason`, `recall.negative_cache` | `crates/pa-recall/src/feature.rs` (a detached root on the crate's own worker, after a top-level session's agent run ends) |
| `recall.witness` | `recall.repo_key`, `recall.has_mark`, `recall.changed`, `recall.changed_unknown`, `recall.unchanged`, `recall.unverifiable`, `recall.uncompared`, `recall.claims_current`, `recall.claims_expired`, `recall.head_moved`, `recall.block_bytes`, `recall.skipped`, `recall.skip_reason`, `recall.negative_cache` | `crates/pa-recall/src/feature.rs` (`after_tool_call` on the session's first `ipython` result, inside its `tool.execute`; 1 s deadline) |
| `recall.digest` | `recall.phase` (`tool_call`/`tool_result`), `recall.repo_key`, `recall.verifiable`, `recall.digest_matched`, `recall.ms`, `recall.skipped`, `recall.skip_reason`, `recall.negative_cache` | `crates/pa-recall/src/feature.rs` (the workspace digest a build claim is checked against, before and after an `ipython` cell that names a build command; 1 s deadline) |
| `toolforge.publish` | `toolforge.name`, `toolforge.import`, `toolforge.status`, `toolforge.reason`, `toolforge.installed`, `toolforge.gate_runs` | `crates/pa-toolforge/src/publish.rs` (`publish`, one per `toolforge.publish` host request) |
| `toolforge.gate` | `toolforge.name`, `toolforge.negative`, `toolforge.positive`, `toolforge.passed` | `crates/pa-toolforge/src/publish.rs` (`run_gate`, the double run) |
| `trajectory.seal` | `windows`, `labelled`, `withheld`, `backfill` | `crates/pa-learning/src/command.rs` (`prime-agent learning trajectory`, counts only) |
| `workflow.run_agent` | none; a `workflow.run_agent settled` event carries the classification | `crates/pa-workflow/src/v1/host.rs` (`handle_run_agent`) |
| `workflow.v2.request` | none; a `workflow.v2.request answered` event carries the action and outcome | `crates/pa-workflow/src/v2/host.rs` |
| `dream.experiment` | `dream.experiment_id`, `dream.task`, `dream.seed`, `dream.rounds`, `dream.arms`, `dream.mode`, `trigger.trace_id`, `dream.stopped`, `error` | `crates/pa-dream/src/experiment.rs` (`experiment_span`: an in-turn root from the CLI, a detached root in a session) |
| `dream.experiment_arm` | `dream.experiment_id`, `dream.arm`, `dream.fixed_policy`, `dream.guided`, `dream.run_id` | `crates/pa-dream/src/experiment.rs` |
| `dream.run` | `dream.task`, `dream.seed`, `dream.workers`, `dream.k1`, `dream.k2`, `dream.dreams`, `dream.iterations`, `dream.mode`, `dream.fixed_policy`, `dream.priming_policies`, `dream.run_id`; in a session also `dream.child_model`, `dream.child_thinking`, `dream.child_max_output_tokens`, `trigger.trace_id`, `dream.stopped` | `crates/pa-dream/src/dream_loop.rs` (local CLI) and `crates/pa-dream/src/llm_loop.rs` (in session, a detached root) |
| `dream.explore` | `dream.policy_id`, `dream.k1`, `dream.workers`, `dream.iteration`, `dream.tree_id` | `crates/pa-dream/src/rollout.rs` (one per rollout) |
| `dream.round` | `dream.round`, `dream.batch_size`, `dream.revealed_count`, `dream.best_score` | `crates/pa-dream/src/rollout.rs` |
| `dream.attempt` | `dream.node_id`, `dream.parent_id`, `dream.task`, `dream.valid`, `dream.score`, `dream.tokens`, `dream.origin`, `dream.fail_class` | `crates/pa-dream/src/rollout.rs` |
| `dream.dream` | `dream.candidates`, `dream.pool_size`, `dream.iteration`, `dream.chosen_policy_id`, `dream.chosen_score`, `dream.current_score`, `dream.chosen_quality`, `dream.current_quality`, `dream.quality_rejected`, `dream.unmeasurable`, `dream.improved`, `dream.dreamer`, `dream.in_support_current`, `dream.measured_trees`, `dream.evidence_trees`, `dream.simulations`, `dream.lever_gap`, `dream.lever_policies`, `dream.lever_simulations` | `crates/pa-dream/src/improve.rs` (one per dreaming step) |
| `dream.replay` | per step: `dream.policy_id`, `dream.iteration`, `dream.simulations`, `dream.measured_trees`; standalone: `dream.policy_id`, `dream.tree_id`, `dream.revealed_n`, `dream.rounds`, `dream.v`, `dream.out_of_support`, `dream.in_support`, `dream.probes_to_best`, `dream.simulations` | `crates/pa-dream/src/improve.rs` (one summary per step) and `crates/pa-dream/src/replay.rs` (`prime-agent dream replay`) |
| `dream.candidate` | `dream.iteration`, `dream.candidate_index`, `dream.policy_id`, `dream.origin`, `dream.reason`, `dream.eligible`, `dream.value`, `dream.quality`, `dream.anytime`, `dream.cost`, `dream.rounds_saved`, `dream.in_support_min`, `dream.charged_probes`, `dream.charged_rounds`, `dream.changed` | `crates/pa-dream/src/improve.rs` (one zero-duration child of `dream.dream` per candidate verdict) |
| `dream.redeploy` | `dream.policy_id`, `dream.k1`, `dream.workers`, `dream.iteration`, `dream.tree_id`, `dream.fixed_policy`, `dream.probation`, `dream.probation_floor`, `dream.reverted` | `crates/pa-dream/src/dream_loop.rs`, `crates/pa-dream/src/llm_loop.rs` |
| `dream.llm_propose` | `dream.round`, `dream.tokens`, `dream.llm_output_tokens`, `dream.llm_attempts`, `dream.llm_fallback`, `dream.origin`, `dream.llm_reject_reason`, `dream.llm_status`, `dream.llm_reject_excerpt` | `crates/pa-dream/src/llm.rs` (`--llm-proposer` only) |
| `dream.llm_dream` | `dream.candidates_requested`, `dream.iteration`, `dream.tokens`, `dream.llm_attempts`, `dream.candidates_returned`, `dream.candidates_dropped`, `dream.candidates_kept`, `dream.candidates_truncated`, `dream.candidates_local`, `dream.candidates`, `dream.dreamer`, `dream.llm_status`, `dream.llm_fallback`, `dream.llm_reject_reason`, `dream.llm_reject_excerpt` | `crates/pa-dream/src/llm.rs` (`--llm-dreamer` only) |
| `dream.llm_guidance` | `dream.iteration`, `dream.pool_size`, `dream.tokens`, `dream.llm_fallback` | `crates/pa-dream/src/llm.rs` (the guided arms of an experiment) |

The meaning of every `dream.*` attribute (the replay objective, verdict
reasons, probation) is in `docs/dream-rsi.md` and `crates/pa-dream/README.md`.

### Kernel runtime spans

Opened by the Python runtime and forwarded to the host's log (see
`prime-agent-runtime/src/rlm/repl.md`, "Trace context"):

| span | attributes | opened in |
|---|---|---|
| `kernel.cell` | `kernel.request_id`, `kernel.request_type` | `prime-agent-runtime/src/rlm/repl.py` (one per `execute`/`snapshot`/`restore`; emits `span_start` too) |
| `kernel.host_request` | `host_request.rid`, `host_request.type` | `prime-agent-runtime/src/rlm/repl.py` (client side of each host request) |
| `bash.command` | `bash.command` (truncated), `bash.pid`, `bash.exit_code`, `bash.signal`, `bash.killed`, `bash.output_bytes`; `span_start` carries `bash.pid`, `bash.pgid`, `bash.started_at` | `prime-agent-runtime/src/rlm/bash.py` (one per `bash()` call; the process runs host-side in `pa-bash`, see Carriers; kernel shutdown ends it as error `"kernel shutdown"`) |
| `mcp.call` | `mcp.server`, `mcp.tool`, `mcp.connected`, `mcp.tool_count` | `prime-agent-runtime/src/rlm/mcp.py` |

`bash.py` also emits the `command_no_output`, `cargo_lock_wait` and
`command_progress` diagnostics through `trace.emit_event`; those three
are the only non-span runtime records the host keeps.

### Annotating a span you do not own

An event under `pa_types::trace_context::SPAN_ATTRIBUTES_TARGET` adds its
fields to the attributes of the span it happens in and writes no line. This
is how `pa-ledger` stamps `failure.fingerprint` on the failed `tool.execute`
(its `after_tool_call` runs inside that span), and how `pa-ravo` adds
`ledger.regressions` and the `trust.*` counts to `harness.ledger.flush`.

## Carriers (how the context crosses a boundary)

| boundary | carrier |
|---|---|
| async continuation in one process | the `tracing` span stack (Rust) / `contextvars` (Python) |
| host -> Python kernel | request frame field `traceparent` (`TRACEPARENT_FIELD`), set from the `kernel.execute` span in `crates/pa-core/src/kernel/manager/requests.rs` |
| kernel process spawn | env `TRACEPARENT`, set from the `kernel.start` span in `crates/pa-core/src/kernel/manager/startup.rs` |
| Python -> host `host_request` | event frame field `traceparent` (the runtime's `kernel.host_request` span); read only by `bash.run`, see below |
| `bash()` command -> its process | the `bash.run` host request's `traceparent` (the `bash.command` span); `pa-bash` puts it in the child's env as `TRACEPARENT` (`crates/pa-bash/src/shell.rs`) |
| kernel runtime -> host log | `{"event":"trace", ...}` frames (kernel protocol 4 and later) |
| external caller -> `prime-agent` | env `TRACEPARENT`, read once at startup by `RecorderConfig::from_env` |
| a span with a remote parent | a span field named `traceparent` (`REMOTE_PARENT_FIELD`) |

## Logging contract

`<agentDir>/logs/agent.jsonl` holds one JSON object per line. An event line
carries `ts`, `level`, `component` (the `tracing` target, e.g.
`pa_ravo::refinement`), `msg`, the event's fields, `pid`, and the
`traceId`/`spanId`/`parentSpanId` of the span it happened in. A span field
`session.id` scopes the `sessionId` of every entry inside that span.

Span completion is itself a line (`component: "trace"`):

```json
{"component":"trace","msg":"span_end","name":"llm.request","traceId":"…","spanId":"…","parentSpanId":"…","durationMs":812,"status":"error","attrs":{"llm.provider":"openai","llm.base_url":"https://api.openai.com/v1"}}
```

* Span fields become `attrs` with their dotted names. A recorded `error`
  field, or an ERROR event carrying only `error` (what
  `#[instrument(err)]` emits), marks the span `status: "error"`.
* An event field named `<key>.json` carries JSON text; the line holds the
  parsed value under `<key>` (`JSON_FIELD_SUFFIX` in `pa_trace`). This is how
  `refinement.committed` writes `addressed` as an array.
* `span_start` is written only for long-running operations, so a crash or a
  hang stays visible without doubling the traffic. The list is
  `ACTIVE_OPERATION_SPANS` in `crates/pa-trace/src/layer.rs`; of its names,
  the Rust host opens `kernel.start`, `session.compact` and `ravo.run`. The
  others are kept so the readers treat lines the TS binary writes into the
  same file the same way.
* Kernel runtime records arrive as `Event::Trace` frames
  (`crates/pa-core/src/kernel/manager/events.rs`), are re-emitted under
  `pa_types::trace_context::FORWARDED_RECORD_TARGET`, and are written
  verbatim when they carry their own ids, with `attrs.error` hoisted. A build
  without `pa-trace` drops them.
* `pa-core`'s request-timing writer
  (`crates/pa-core/src/session_engine/request_timing.rs`) appends to the same
  file without the rotation lock.

## Refinement outcome records

A refinement reports its final decision once, as one line under the target
`pa_ravo::refinement` (`crates/pa-ravo/src/outcome.rs`,
`log_refinement_outcome`), written for gated `/refine` and for each
proposal a `ravo.run` evaluates:

| `msg` | fields | written for |
|---|---|---|
| `refinement.committed` | `proposalId`, `addressed`, `deepScore`, `missed`, `reason`, `scope` | a `commit` that claimed at least one fingerprint; the learning index's treated cohort |
| `refinement.applied_unmeasured` | `proposalId`, `deepScore`, `reason`, `scope` | `commit_unmeasured`, `rollback`, and a `commit` with nothing addressed |
| `refinement.rejected` | `proposalId`, `decision`, `deepScore`, `missed`, `claimed`, `reason`, `scope`, `cause` (`reject_*` only) | every other decision |

`ravo.run` writes `reason: "ravo_run"`. `prime-agent learning` reads only
`refinement.committed` (`crates/pa-learning/src/index.rs`), matched by `msg`.

## Harness trust records

Each trust move is one line under the target `pa_ravo::harness_trust`
(`crates/pa-ravo/src/trust.rs`, `log_trust_settlement`), written where a
trust window settles (a ledger flush, or a refine's apply):

| `msg` | fields | written for |
|---|---|---|
| `harness.trust.settled` | `proposal_id`, `scope`, `from`, `outcome` (`clean`/`contested`/`faulted`), `ordinal`, `fingerprints` (comma-joined) | one per window the settlement closed |
| `harness.trust.adjusted` | `proposal_id`, `scope`, `entry` (`kind:id`), `reason`, `delta`, `before`, `after`, `dormant`, `fingerprint_id` (on a fault) | one per entry whose score moved |

`delta` is `+5` for a clean window, charged to every entry the commit wrote,
and `-15` for a measured fault, charged once per window to the skill entry
the replay ran for; `dormant` is true below 30, where the entry leaves the
rendered harness digest. The learning index reads neither line.

## Switches

Each is on unless set as shown:

* `PRIME_AGENT_GLOBAL_LEDGER=0|off|false|no`: the failure ledger stays per
  session, and no `harness.ledger.flush` span is opened
  (`crates/pa-ledger/src/harness.rs`). Trust windows are measured on the
  global ordinal, so trust adjudication needs it on.
* `PRIME_AGENT_RAVO=0|off|false`: no refinement gate, no `ravo.*` host
  requests, no refinement outcome lines (`crates/pa-ravo/src/feature.rs`).
* `PRIME_AGENT_WORKSPACE_RECALL=0|off|false|no`: no `recall.mark`,
  `recall.witness` or `recall.digest` span and no mark (`crates/pa-recall/src/feature.rs`).

A Workspace Recall mark is
`<agentDir>/recall/<basename>.<sha256(repoRoot)[:16]>.json`; that key
(`recall.repo_key`, computed by `recall_repo_key`) is the repo root's
basename, a dot, and the first 16 hex characters of the sha256 of the
resolved root path. It holds digests (`digestAlgorithm: "sha256-128"`),
paths, HEAD and build claims, never file content or command output. A git
call that times out writes `<basename>.<hash>.skip.json` (reason
`git_timeout`) for 10 minutes, and every process sharing the agent dir then
skips that repo with `recall.negative_cache` set; a missed 1 s tool-path deadline keeps
only this process off the repo, in memory, for 60 s.

## Python runtime (`prime-agent-runtime/src/rlm/trace.py`)

* `parse_traceparent(str) -> TraceContext | None`, `format_traceparent(ctx)`.
* `current() -> TraceContext | None` (contextvar backed).
* `start_span(name, **attrs) -> Span` context manager: mints a child span,
  sets it current, restores the parent on exit, and hands a `span_end` event
  to the installed emitter; `rlm.repl` ships it to the host as
  `{"event":"trace", ...}`.
* `emit_event(component, msg, **fields)` for diagnostics in the same shape.
* `inject_env(env) -> dict` sets `TRACEPARENT` for subprocesses;
  `from_env(env)` reads it.
* Optional bridge: if `opentelemetry` is importable, the current context is
  also attached to the OTel context, so user code using the OTel SDK sees
  the same trace; its absence is not an error.
* `rlm.repl` reads `traceparent` from every `execute`/`snapshot`/`restore`
  request, runs the cell under `start_span("kernel.cell")`, and stamps
  `traceparent` into every `host_request` frame it emits.

Kernel protocol 4 added the runtime's `trace` and `host_cancel` events; the
host parses them (`crates/pa-core/src/kernel/protocol.rs`,
`REPL_PROTOCOL_VERSION` is now 5). A native build logs `trace` frames at
debug and records nothing.

## How to use

### Find a trace id

* From a log line: every entry written while a span was active has
  `"traceId":"<32 hex>"`. For example, the trace of the most recent failed
  provider request:

  ```sh
  grep '"name":"llm.request"' ~/.prime/agent/logs/agent.jsonl | grep '"status":"error"' | tail -1 | grep -o '"traceId":"[0-9a-f]*"'
  ```

* From a kernel frame or a subprocess environment: the `traceparent` /
  `TRACEPARENT` value can be passed to `prime-agent trace` as it is.

### `prime-agent trace`

```
prime-agent trace <traceId|traceparent> [--log <path>] [--json]
```

Reads every retained generation of the log, oldest first
(`pa_trace::retained_log_files`: the `.old.<n>.gz` generations, `.old`, then
the live file), keeps the entries for one trace id and prints a tree
(`crates/pa-trace/src/trace_command.rs`). The rendering is pinned by the
test `renders_nested_spans_attributed_log_lines_and_orphans`; on its fixture:

```
trace 0af7651916cd43dd8448eb211c80319c  (3 spans, 3 log lines, /tmp/agent.jsonl)
├─ agent.turn  1050ms  ok  session.id=abc turn.index=1  [b7ad6b7169203331]
│  ├─ 10:00:00.100  info   session  turn started  sessionId=abc
│  ├─ llm.request  750ms  error  llm.provider=openai llm.base_url=https://api.example.test/v1  error=401 archived  [c8be7c8270314442]
│  │  └─ 10:00:00.200  info   ai.provider  request  baseUrl=https://api.example.test/v1
│  └─ tool.execute  50ms  ok  tool.name=bash tool.call_id=call_1  [d9cf8d9381425553]
└─ (no span)
   └─ 10:00:01.200  debug  daemon  context only
```

* Spans are the `span_end` entries nested by `parentSpanId`, with name,
  `durationMs`, status, every attribute and the recorded error. The trailing
  span id in brackets lets you grep the raw log for one span.
* Log lines sit under the span whose `spanId` they carry, interleaved with
  child spans in time order; every other field is shown as `key=value`.
* A span that has not ended (still running, rotated away, or owned by an
  external caller) is shown as `(open span)`, using its `span_start` when
  one was written, so its children and lines stay grouped.
* Lines with the trace id but no span id go under `(no span)`.
* `--json` prints the raw matching lines in file order.
* `--log <path>` reads another log and its retained generations.
* Exit code 1 with an `Error:` line on stderr when the id is malformed, the
  log does not exist, or nothing matched.

### `prime-agent health`

```
prime-agent health [--since <duration>] [--stuck-after <duration>] [--limit <n>] [--log <path>] [--json]
```

Reads the retained generations without contacting the daemon
(`crates/pa-trace/src/health.rs`) and summarizes recent incidents by class:
historian, provider (failed `llm.request` spans and `provider stream
failure` lines, deduplicated by trace), stuck turns, daemon recovery,
process, kernel, child, message delivery, lock, orphan and diagnostic.

The default window is 24 hours, the stuck threshold 10 minutes, and at most
20 incident details are printed (hard maximum 200). Counts always cover the
whole window; open-span correlation is kept apart from the 100,000-entry
analysis buffer, so a busy log does not hide an old open operation.

`--json` includes `status` (`healthy`, `unhealthy`, `unknown`),
`parseErrors`, `stale`, the counts and the bounded incident list. Exit
status is 0 only for valid recent evidence with no incidents, 2 for
incidents or unknown evidence (malformed, empty or stale), and 1 for usage
and read errors. It is a retained-log heuristic, not a live probe: rotation
can remove a span's end and leave a false open-operation candidate. Several
classes match records only the TS binary writes (see below), so on a
Rust-only machine they read zero. Timestamps parse with
`pa_types::incident::timestamp_to_ms`, shared with `prime-agent incident`.

Both readers record the `observability command used` adoption event
(`command`: `trace`/`health`, `outcome`), emitted by `pa-cli`.

### Parenting Prime Agent from outside

Any process that starts `prime-agent` can hand it a span through the W3C
environment carrier; every root span of that process becomes its child:

```sh
TRACEPARENT=00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01 prime-agent -p "summarize the failing test"
prime-agent trace 0af7651916cd43dd8448eb211c80319c
```

`RecorderConfig::from_env` reads the variable once; a malformed value is
ignored. The context does not cross the daemon socket (see below), so this
covers the process it was given to.

## Optional OTLP export and derived metrics

Set `OTEL_EXPORTER_OTLP_ENDPOINT` to turn it on (`crates/pa-trace/src/otlp.rs`);
`OTEL_EXPORTER_OTLP_HEADERS` optionally provides comma-separated
`key=value` request headers (`parse_otlp_headers`). Unset, there is no
worker, queue or network call.

Finished spans and per-name delta metrics (`prime_agent.span.count`,
`prime_agent.span.error_count`, `prime_agent.span.duration_ms`, at most 256
span names) are posted as OTLP/HTTP JSON to `<endpoint>/v1/traces` and
`<endpoint>/v1/metrics`: batches of 128, at most 2,048 queued spans (oldest
dropped, counted in `OtlpStats`), a flush every 5 s, a 10 s request timeout.
Credential-looking attribute keys are dropped. Recording only queues; one
background worker owns delivery, so a hanging collector never delays traced
code. An orderly exit drains for at most one second (`SHUTDOWN_DRAIN`).

## Local log safety and retention

All recorder writes go through `crates/pa-trace/src/log_file.rs`:

* the logs directory is `0700` and log files `0600`;
* high-confidence credentials (bearer tokens, API keys, refresh and access
  tokens, client secrets, password assignments, JWTs, common provider token
  forms) are redacted to `[REDACTED]` before disk;
* rotation at 20 MiB is serialized across processes by the
  `agent.jsonl.rotation-lock` file and its `.lock` directory, the same lock
  the TS logger used (a lock older than 10 s is reclaimed);
* the live file and the newest `.old` stay plain text, older generations are
  `.old.<n>.gz`; five generations are kept by default, and
  `PRIME_AGENT_LOG_RETENTION` (1 to 100) changes the bound;
* writes go through one background thread behind a bounded queue, so no
  traced path waits on the disk, and a process that records nothing opens no
  file.

`prime-agent trace`, `prime-agent health` and `prime-agent learning` read
all retained generations (gzip bounded at 64 MiB per generation).

## Differences from the TS fork

These are things the TS implementation did that the Rust host does not, kept
here so the history of why stays readable. "Not ported" means the code does
not exist in this tree.

* **Not ported: spans with no Rust equivalent.** `client.prompt`,
  `client.turn`, `daemon.command`, `agent.prompt`, `agent.retry`,
  `tool.prepare`, `extension.hooks`, `extensions.load`, `rlm.child`,
  `rlm.child.run`, `rlm.run_agent`, `cron.job`, `oauth.refresh`,
  `trace.upload`, `package.install`, `package.remove`, `package.update`,
  `package.check_updates`, `package.command`, `update.check`, `update.self`,
  `tools.download`, `tools.release_lookup`, and the bridged
  `context.transform`, `historian.run` and `auth.refresh` family of spans
  from Magic Context and the anthropic-auth plugin. `pa-anthropic-auth` emits no
  spans. The `health` reader still recognises these names, because the TS
  binary may write into the same log.
* **Not ported: the refine spans.** `refine.plan`, `refine.apply`,
  `ravo.referee`, `ravo.replay_case` and `ravo.replay_verify` do not exist:
  the refine path in `pa-core` opens no span, and the referee and replay
  self-checks run unspanned. Their attributes (`refine.*`, `referee.*`,
  `refine.trust_window_opened`) are gone with them, and
  `toolforge.gate` has no `ravo.replay_case` children.
* **Not ported: `trigger.trace_id` on most detached roots.** Only the
  in-session `dream.experiment` and `dream.run` carry it. `recall.mark` and
  `harness.trust.adjudicate` (which runs on the ledger's worker thread,
  outside the turn) do not.
* **Not ported: cross-process carriers beyond the kernel.** No
  `traceparent` on daemon command envelopes or worker-protocol frames, and
  no `TRACEPARENT` for an RLM child session; a trace stops at the daemon
  socket. A `host_request` frame's `traceparent` is read only by `bash.run`;
  other handlers do not parent on it.
* **Not ported: session record stamping.** Session JSONL entries carry no
  `traceId`/`spanId` (TS `stampTraceContext`).
* **Not ported: crash and exit records.** The structured fatal-crash and
  unexpected-kernel-exit lines the TS host wrote are not written; `health`
  only reads them.
* **Not ported: refinement history files and the stale-evidence re-plan.**
  No `refinement.history_append_failed` / `refinement.history_unreadable`
  lines, no `<agentDir>/harness/refinements.jsonl` or
  `local-refinements/<sessionId>.jsonl`, and no `staleEvidence`,
  `driftKind`, `replanOf` fields (see `crates/pa-ravo/README.md`,
  Non-goals).
* **Changed: log components.** Events use their `tracing` target as
  `component` (`pa_ravo::refinement`, `pa_ravo::harness_trust`) where TS
  used `coding-agent.refinement` and `coding-agent.harness-trust`. Readers
  match on `msg`.
* **Changed: trust line field names.** The `harness.trust.settled` and
  `harness.trust.adjusted` fields are
  snake_case (`proposal_id`, `fingerprint_id`) and `fingerprints` is a
  comma-joined string; TS wrote `proposalId`, `fingerprintId` and an array.
* **Changed: OTLP wiring.** TS exposed a programmable
  `createOtlpSpanExporter` / `addSpanSink` adapter with configurable bounds;
  the Rust exporter is configured only by the two environment variables.

## Non-goals

* No `opentelemetry` dependency; the OTLP/HTTP JSON exporter is built in.
* No collector is contacted unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
* Derived metrics are process-local deltas exported through OTLP; there is
  no metrics database, dashboard server or alerting engine.
