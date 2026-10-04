# pa-trace

Observability for Prime Agent (fork feature crate, `docs/fork-feature-crates.md`): the span recorder that writes the
shared structured log, the `prime-agent trace` and `prime-agent health` readers over it, and the optional OTLP/HTTP
exporter. Behavioural spec: the fork's `docs/observability.md` (TS branch `perf/session-catalog-resume`).

## Scope

- **Recorder** (`TraceLayer`): a `tracing_subscriber` layer. Every recorded span gets a W3C context (a child of its
  parent span, of a `traceparent` field, or of the inbound `TRACEPARENT`; else a new trace). It writes the TS
  `agent.jsonl` records: `span_end` per finished span (`name`, `durationMs`, `status`, `attrs`, `error`),
  `span_start` for the long-running operations (`client.turn`, `agent.prompt`, `kernel.start`, `session.compact`,
  `cron.job`, ...), one line per event stamped with `traceId`/`spanId`/`parentSpanId`, and the kernel runtime's own
  spans forwarded verbatim (TS `forwardKernelTraceEvent` rules: ids required, `attrs.error` hoisted, only
  `command_no_output`/`cargo_lock_wait`/`command_progress` diagnostics kept). It answers
  `pa_types::trace_context::current()`, so native code sets `TRACEPARENT` / `traceparent` carriers.
- **Log file**: TS `appendRotatingLog` semantics: high-confidence credential redaction before disk, `0600` files in a
  `0700` directory, the `<path>.rotation-lock.lock` cross-process lock, 20 MiB rotation keeping `<path>.old` plain and
  older generations as `<path>.old.<n>.gz`, `PRIME_AGENT_LOG_RETENTION` (1..=100, default 5) generations. Writes go
  through one background thread behind a bounded queue: no traced path waits on the disk, and a process that records
  nothing starts no thread and opens no file.
- **`prime-agent trace <traceId|traceparent> [--log <path>] [--json]`**: one trace rebuilt as a span tree with its
  log lines, across all retained generations (TS `cli/trace-command.ts`).
- **`prime-agent health [--since <d>] [--stuck-after <d>] [--limit <n>] [--log <path>] [--json]`**: the bounded
  incident summary (TS `cli/health-command.ts`); exit 0 healthy, 2 incidents or unknown evidence, 1 usage/read error.
  Timestamps parse with `pa_types::incident::timestamp_to_ms` (shared with `prime-agent incident`).
- **OTLP** (only when `OTEL_EXPORTER_OTLP_ENDPOINT` is set; `OTEL_EXPORTER_OTLP_HEADERS` optional): finished spans
  and per-name delta metrics (`prime_agent.span.count`, `.error_count`, `.duration_ms`) as OTLP/HTTP JSON to
  `<endpoint>/v1/traces` and `/v1/metrics`, batches of 128, at most 2,048 queued (oldest dropped), flushed every 5 s,
  10 s request timeout, credential-looking attribute keys dropped. Recording only queues; one background worker owns
  delivery. An orderly exit drains for at most one second.

## Non-goals

No `opentelemetry` dependency, no collector contact unless configured, no metrics database. Session-file
`traceId`/`spanId` stamping, the daemon envelope carrier, and the remaining contract spans live in native crates
(see the porting report).

## Public API

`recorder(RecorderConfig) -> (TraceLayer, RecorderHandle)`, `install(RecorderConfig)` (global subscriber + context
source), `install_context_source()`, `RecorderConfig::from_env`, `RecorderHandle::{flush, shutdown, otlp_stats}`,
`run_trace_command`, `run_health_command`, `CommandOutcome`, `OtlpConfig`, `OtlpStats`, `parse_otlp_headers`,
`SHUTDOWN_DRAIN`, `OTLP_ENDPOINT_ENV`, `OTLP_HEADERS_ENV`.

## Seams

- `tracing` spans/events emitted by native crates (only `pa_*` targets at INFO and above are recorded; everything
  else is disabled at the callsite).
- `pa_types::trace_context`: the context source (`set_current_context_source`), the forwarded-record target
  (`FORWARDED_RECORD_TARGET`, field `record`), and the remote-parent span field (`traceparent`).
- `pa-cli`: installs the recorder and wires `trace` / `health` behind `feature = "trace"`.

Field conventions for native spans: span fields become `attrs` (dotted names kept); a recorded `error` field, or an
ERROR event carrying only `error` (what `#[instrument(err)]` emits), marks the span failed; `session.id` scopes the
`sessionId` of every entry inside the span.

## Files

`<agentDir>/logs/agent.jsonl`, its `.old` / `.old.<n>.gz` generations, and the `agent.jsonl.rotation-lock` file and
`.lock` directory: the same files and lock the TS logger used. The native request-timing writer
(`pa-core` `request_timing.rs`) appends to the same file without the lock.

## Telemetry

`observability command used` (`command`: `trace`/`health`, `outcome`), emitted by `pa-cli` when a reader command
runs.

## JSON-valued fields

An event field named `<key>.json` carries JSON text; the record holds the value it parses to under `<key>` (how a
feature writes the array or object a TS log record held, e.g. `pa-ravo`'s `addressed` list). Text that does not parse
stays a string under the full field name.
