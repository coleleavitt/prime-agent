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

All hops below are implemented; the last column names the file that opens
the span so `grep withSpan`/`start_span` lands on it.

| span                  | attributes                                                   | status | created in |
|-----------------------|---------------------------------------------------------------|--------|------------|
| `agent.prompt`        | `session.id`                                                  | done   | coding-agent `core/agent-session.ts` |
| `agent.turn`          | `session.id`, `turn.index`                                    | done   | pi-agent-core `agent-loop.ts` |
| `llm.request`         | `llm.provider`, `llm.api`, `llm.model`, `llm.base_url`        | done   | pi-ai `stream.ts` |
| `tool.execute`        | `tool.name`, `tool.call_id`                                   | done   | pi-agent-core `agent-loop.ts` |
| `kernel.execute`      | `kernel.request_id`, `kernel.request_type`, `kernel.status`   | done   | coding-agent `core/kernel/repl-manager.ts` |
| `kernel.cell`         | `kernel.request_id`                                           | done   | Python `rlm/repl.py` |
| `kernel.host_request` | `host_request.rid`, `host_request.type`                       | done   | Python `rlm/repl.py` (client) / TS `core/kernel/repl-manager.ts` (server) |
| `rlm.child`           | `rlm.child_id`, `rlm.depth`                                   | done   | coding-agent `modes/daemon/daemon-mode.ts` |
| `daemon.command`      | `daemon.request_id`, `daemon.command_type`                    | done   | coding-agent `modes/daemon/daemon-mode.ts` |

Supporting pieces:

| piece                                   | status | file |
|-----------------------------------------|--------|------|
| trace context API + `withSpan`          | done   | pi-ai `trace-context.ts` |
| log stamping + `span_end` entries       | done   | pi-ai `log.ts` |
| `AsyncLocalStorage` install, `TRACEPARENT` inbound read | done | coding-agent `core/logging.ts` (`withInboundTraceContext`, called from `main.ts`) |
| worker/daemon envelope `traceparent`    | done   | coding-agent `modes/daemon/daemon-protocol.ts`, `daemon-worker-protocol.ts`, `daemon-worker-client.ts` |
| Python context + OTel bridge            | done   | `prime-agent-runtime/src/rlm/trace.py` |
| session records stamped `traceId`/`spanId` | done | coding-agent `core/session-manager.ts` (`stampTraceContext` in the single append path) |
| `prime-agent trace` viewer              | done   | coding-agent `cli/trace-command.ts` (registered in `cli/command-registry.ts` + `cli/public-command.ts`) |

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

## Session records

Every entry appended to a session file (`~/.prime/agent/sessions/<id>.jsonl`:
messages, custom entries, model changes, compactions, ...) carries optional
`traceId` and `spanId` copied from the span active at append time. They are
omitted when no span is active, so files written outside a traced turn are
unchanged and older readers keep working (unknown keys are ignored on load).
`parentSpanId` is deliberately not stored: the session record is a join key
into the log, not a second span store.

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

## How to use

### Find a trace id

* From a log line: every entry in `~/.prime/agent/logs/agent.jsonl` written
  while a span was active has `"traceId":"<32 hex>"`. For example, to find
  the trace behind the most recent provider failure:

  ```sh
  grep '"provider stream failure"' ~/.prime/agent/logs/agent.jsonl | tail -1 | grep -o '"traceId":"[0-9a-f]*"'
  ```

* From a session file: the message/turn records in
  `~/.prime/agent/sessions/<sessionId>.jsonl` carry the same `traceId`
  (and the `spanId` of the turn that produced them):

  ```sh
  grep -o '"traceId":"[0-9a-f]*"' ~/.prime/agent/sessions/<sessionId>.jsonl | sort | uniq -c
  ```

* From a kernel frame, a daemon envelope or a subprocess environment: the
  `traceparent` / `TRACEPARENT` value can be passed to the command as-is; the
  trace id is extracted from it.

### `prime-agent trace`

```
prime-agent trace <traceId|traceparent> [--log <path>] [--json]
```

Reads `~/.prime/agent/logs/agent.jsonl` and its rotated sibling
`agent.jsonl.old` (see `appendRotatingLog` in `config.ts`; the writer keeps a
single previous generation), keeps the entries for one trace id and prints a
tree:

```
trace 0af7651916cd43dd8448eb211c80319c  (3 spans, 3 log lines, /home/me/.prime/agent/logs/agent.jsonl)
├─ agent.turn  1050ms  ok  session.id=abc turn.index=1  [b7ad6b7169203331]
│  ├─ 10:00:00.100  info   session  turn started  sessionId=abc pid=4242
│  ├─ llm.request  750ms  error  llm.provider=openai llm.base_url=https://api.openai.com/v1  error=401 archived  [c8be7c8270314442]
│  │  └─ 10:00:00.200  info   ai.provider  request  baseUrl=https://api.openai.com/v1 pid=4242
│  └─ tool.execute  50ms  ok  tool.name=bash tool.call_id=call_1  [d9cf8d9381425553]
└─ (no span)
   └─ 10:00:01.200  debug  daemon  context only  pid=4242
```

* Spans are the `span_end` entries, nested by `parentSpanId`, showing name,
  `durationMs`, status, every attribute (`llm.provider`, `llm.base_url`,
  `tool.name`, `kernel.request_id`, `rlm.child_id`, ...) and the recorded
  error. The trailing `[spanId]` lets you grep the raw log for one span.
* Log lines sit under the span whose `spanId` they carry, interleaved with
  child spans in time order; every other field on the line (including `pid`,
  so cross-process traces stay attributable) is shown as `key=value`.
* A span that has not ended yet (the turn is still running, or its end was
  rotated away, or it belongs to an external caller) is shown as
  `(open span) <spanId>` so its children and lines are still grouped.
* Lines that carry the trace id but no span id go under `(no span)`.
* `--json` prints the raw matching log lines (oldest file first) for piping
  into `jq`.
* `--log <path>` reads another log file (a copy from another machine; its
  `<path>.old` sibling is read too when present).
* Exit code 1 with an `Error:` line on stderr when the id is malformed, the
  log does not exist, or nothing matched.

### Parenting Prime Agent from outside

Any process that starts `prime-agent` can hand it a span through the W3C
environment carrier; everything the run traces then becomes a child of that
span and shares its trace id:

```sh
TRACEPARENT=00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01 prime-agent -p "summarize the failing test"
prime-agent trace 0af7651916cd43dd8448eb211c80319c
```

`main.ts` reads the variable once at startup (`withInboundTraceContext` in
`core/logging.ts`); a malformed value is ignored rather than failing the
run. The same variable is what Prime Agent sets when it spawns the Python
kernel (`injectTraceparentEnv` in `core/kernel/repl-manager.ts`), and daemon
command envelopes carry the equivalent `traceparent` field, so a trace id
chosen by CI or by a parent agent is the one that appears on every log line,
session record and kernel cell below it.

## Non-goals (for now)

* No OTLP exporter and no `@opentelemetry/*` dependency in the core
  packages. A bridge can subscribe to `span_end` log entries.
* No metrics; only traces + correlated logs.
