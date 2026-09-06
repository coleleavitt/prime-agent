# Observability: end-to-end trace context

Status: implemented incrementally on branch `fix/forkserver-probe-hardening`.

## Goal

Every log line, every provider request, every tool execution, every Python
kernel cell, every host request and every RLM child session carries one
**W3C `traceparent`** so a single user turn can be followed across
processes with one id. No third-party dependency is required; the format
and the API surface mirror OpenTelemetry so an OTel SDK / OTLP exporter can
be bridged in without touching call sites.

## Identity

`traceparent = "00-" + traceId(32 hex) + "-" + spanId(16 hex) + "-" + flags(2 hex)`

* `traceId` is minted once per user turn (or inherited from the `TRACEPARENT`
  environment variable when the process is started by an external caller).
* Every hop creates a child span: new `spanId`, same `traceId`,
  `parentSpanId = caller spanId`.
* `flags` is `01` (sampled) by default.

## Span names (stable, dotted)

| span                | attributes                                                   | created in |
|---------------------|---------------------------------------------------------------|------------|
| `agent.turn`        | `session.id`, `turn.index`                                    | pi-agent-core `agent-loop.ts` |
| `llm.request`       | `llm.provider`, `llm.api`, `llm.model`, `llm.base_url`        | pi-ai `stream.ts` |
| `tool.execute`      | `tool.name`, `tool.call_id`                                   | pi-agent-core `agent-loop.ts` |
| `kernel.execute`    | `kernel.request_id`                                           | coding-agent `repl-manager.ts` |
| `kernel.cell`       | `kernel.request_id`                                           | Python `rlm/repl.py` |
| `kernel.host_request` | `host_request.rid`, `host_request.type`                     | Python `rlm/repl.py` (client) / TS `repl-manager.ts` (server) |
| `rlm.child`         | `rlm.child_id`, `rlm.depth`                                   | coding-agent daemon subagent host |
| `daemon.command`    | `daemon.request_id`, `daemon.command_type`                    | daemon worker |

## Carriers (how the context crosses a boundary)

| boundary                         | carrier                                      |
|----------------------------------|----------------------------------------------|
| async continuation in one process| `AsyncLocalStorage` (TS) / `contextvars` (Py)|
| TS -> Python kernel               | JSONL request frame field `traceparent`      |
| Python -> TS `host_request`       | event frame field `traceparent`              |
| kernel process spawn              | env `TRACEPARENT`                            |
| daemon supervisor -> worker       | command envelope field `traceparent`         |
| child session spawn (rlm)        | worker env `TRACEPARENT` + create command    |
| external caller -> prime-agent   | env `TRACEPARENT` (read once at startup)     |

## Logging contract

Every `LogEntry` written to `~/.prime/agent/logs/agent.jsonl` gains
`traceId`, `spanId` and, when present, `parentSpanId`, filled from the
active context at emit time. Span completion is itself a log entry:

```json
{"component":"trace","msg":"span_end","name":"llm.request","traceId":"…","spanId":"…","parentSpanId":"…","durationMs":812,"status":"error","attrs":{"llm.provider":"openai","llm.base_url":"https://api.openai.com/v1"}}
```

Provider failures (`ai.provider` / `provider stream failure`) additionally
log `baseUrl` so a mis-routed request is visible from the failure line
alone.

## Python runtime (`prime-agent-runtime/src/rlm/trace.py`)

* `parse_traceparent(str) -> TraceContext | None`, `format_traceparent(ctx)`.
* `current() -> TraceContext | None` (contextvar backed).
* `start_span(name, **attrs) -> Span` context manager: mints a child span,
  sets it current, restores parent on exit, and emits a `span_end` trace
  event through the repl event stream (`{"event":"trace", ...}`) so the host
  writes it to the shared log with the same shape as TS spans.
* `inject_env(env: dict) -> dict` sets `TRACEPARENT` for subprocesses.
* Optional bridge: if `opentelemetry` is importable, the current context is
  also attached to the OTel context so user code that uses the OTel SDK
  sees the same trace; absence of the package is not an error.
* `rlm.repl` reads `traceparent` from every `execute`/`snapshot`/`restore`
  request, runs the cell under `start_span("kernel.cell")`, and stamps
  `traceparent` into every `host_request` frame it emits.

## Non-goals (for now)

* No OTLP exporter and no `@opentelemetry/*` dependency in the core
  packages. A bridge can subscribe to `span_end` log entries.
* No metrics; only traces + correlated logs.
