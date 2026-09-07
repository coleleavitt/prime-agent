# Observability: end-to-end trace context

Status: implemented incrementally on branch `fix/forkserver-probe-hardening`.

## Goal

Every log line, every provider request, every tool execution, every Python
kernel cell, every host request and every RLM child session carries one
**W3C `traceparent`** so a single user turn can be followed across
processes with one id. No third-party dependency is required; the format and the API surface mirror
OpenTelemetry. An optional dependency-free OTLP/HTTP JSON adapter can subscribe
to the same span sink without changing call sites.

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
| `client.prompt`       | `client.command`, `client.source`, `client.queue_if_busy`, `session.active_id` | done | coding-agent `modes/agent-connection/daemon-agent-connection.ts` (TUI/CLI side root; worker spans nest under it) |
| `tool.prepare`        | `tool.name`, `tool.call_id`, `tool.blocked`, `tool.block_reason` | done | pi-agent-core `agent-loop.ts` (argument validation + `beforeToolCall`: permission prompts live here) |
| `extension.hooks`     | `hook.event`, `hook.handlers`, `hook.slowest`, `hook.slowest_ms`, `hook.errors`, `hook.<ext>_ms` (>25 ms) | done | coding-agent `core/extensions/runner.ts` (one span per emit, skipped when no handler) |
| `session.compact`     | `session.id`, `llm.provider`, `llm.model`, `compact.tokens_before`, `compact.summary_chars`, `compact.first_kept_entry` | done | coding-agent `core/agent-session.ts` (manual and automatic) |
| `agent.retry`         | `retry.attempt`, `retry.max_attempts`, `retry.delay_ms`, `retry.error` | done | coding-agent `core/agent-session.ts` (backoff wait before re-issuing a failed turn) |
| `rlm.run_agent`       | `rlm.requested_model`, `rlm.model`, `rlm.status`, `rlm.turns` | done | coding-agent `core/agent-session.ts` (`ctx.runAgent` children) |
| `cron.job`            | `cron.job_id`, `cron.name`, `cron.kind`, `cron.runtime_kind`, `cron.session_id`, `cron.deferred`, `cron.delivery`, `cron.result` | done | coding-agent `modes/daemon/daemon-mode.ts` (scheduled/heartbeat prompts nest their `agent.prompt` under it) |
| `context.transform`   | `context.messages_in/out`, `context.targets`, `context.usage_percent`, `context.input_tokens`, `context.context_limit` | done | Magic Context `pi-plugin/src/context-handler.ts` (optional pi-ai bridge) |
| `client.turn`         | `session.active_id`, `client.source`, `turn.queued`, `turn.messages` | done | coding-agent `modes/agent-connection/daemon-agent-connection.ts` (submit → `agent_end` seen by the window; error on close/dispose/rejected admission) |
| `oauth.refresh`       | `oauth.provider`, `oauth.expired_ms`, `oauth.outcome` | done | coding-agent `core/auth-storage.ts` (refresh under the auth-file lock; a failure is otherwise swallowed into "no API key") |
| `trace.upload`        | `upload.status`, `upload.bytes`, `http.status` | done | coding-agent `core/agent-traces.ts` |
| `kernel.start`        | `kernel.python`, `kernel.restore`, `kernel.bootstrapped`, `kernel.python_ms`, `kernel.python_path` (`stamped`/`verified`/`synced`/`bootstrapped`/`override`), `kernel.pid` | done | coding-agent `core/kernel/repl-manager.ts` (the child's `TRACEPARENT` names this span) |
| `extensions.load`     | `extensions.count`, `extensions.loader_ms`, `extensions.errors`, `extensions.slowest`, `extensions.slowest_ms`, `extensions.<label>_ms` (>100 ms) | done | coding-agent `core/extensions/loader.ts` |
| `session.load`        | `session.path`, `session.bytes`, `session.entries` | done | coding-agent `core/session-manager.ts` (`open`/`openAsync`) |
| `bash.command`        | `bash.command`, `bash.pid`, `bash.exit_code`, `bash.signal`, `bash.killed`, `bash.output_bytes` | done | Python `rlm/bash.py` (the child's `TRACEPARENT` names this span; kernel shutdown ends it as error "kernel shutdown") |
| `mcp.call`            | `mcp.server`, `mcp.tool`, `mcp.connected`, `mcp.tool_count` | done | Python `rlm/mcp.py` |
| `ravo.run`            | `ravo.run_id`, `ravo.resumed`, `ravo.reason`, `ravo.rounds`, `ravo.repairs`, `ravo.spent_tokens`, `ravo.certificate_digest` | done | coding-agent `core/ravo/controller.ts` (deadline/budget/cancel are ok + reason) |
| `ravo.round`          | `ravo.round`, `ravo.phase`, `ravo.outcome`, `ravo.reason` | done | coding-agent `core/ravo/controller.ts` |
| `ravo.proposal`       | `ravo.round`, `ravo.kind`, `ravo.proposal_id`, `ravo.candidate_tokens` | done | coding-agent `core/ravo/controller.ts` (implement/repair child call) |
| `ravo.evaluation`     | `ravo.proposal_id`, `ravo.evaluator`, `ravo.evaluator_kind`, `ravo.verdict`, `ravo.certificate_digest` | done | coding-agent `core/ravo/controller.ts` (each evaluator + the commit gate) |
| `package.install` / `package.remove` / `package.update` / `package.check_updates` | `package.source`, `package.local`, `package.count`, `package.updates` | done | coding-agent `core/package-manager.ts` |
| `package.command`     | `command` (program + first arg), `exit_code`, `signal` | done | coding-agent `core/package-manager.ts`, `package-manager-cli.ts` (nested git/npm child processes) |
| `update.check`        | `update.current`, `update.latest`, `update.available`, `http.status` | done | coding-agent `utils/version-check.ts` |
| `update.self`         | `update.from`, `update.to` | done | coding-agent `package-manager-cli.ts` |
| `tools.download` / `tools.release_lookup` | `tool`, `version`, `bytes`, `tool.repo`, `http.status` | done | coding-agent `utils/tools-manager.ts` |
| `historian.run` / `historian.subagent` / `historian.validate` / `historian.publish` | `historian.session_id`, `historian.chunk_start/end`, `historian.model`, `historian.status` (run), `historian.pass`, `historian.outcome` (subagent), `historian.valid`, `historian.compartments`, `historian.facts`, `historian.failure_reason` | done | Magic Context `packages/pi-plugin/src/pi-historian-runner.ts` (via the optional pi-trace bridge) |
| `auth.refresh` / `auth.catalog` / `auth.route` | `auth.reason`, `auth.account`, `auth.source`, `auth.outcome`, `http.status`, `catalog.models`, `catalog.cached`, `auth.pool_size`, `auth.selected` | done | anthropic-auth `packages/pi/src/{shared-refresh,index,stream}.ts` (via `trace-bridge.ts`) |

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

### `prime-agent health`

```
prime-agent health [--since <duration>] [--stuck-after <duration>] [--limit <n>] [--log <path>] [--json]
```

Reads the retained compressed generations, `agent.jsonl.old`, and `agent.jsonl`
without contacting the daemon. It gives operators a bounded summary of recent:

* failed `historian.*` spans;
* provider stream failures and failed `llm.request` spans (deduplicated by trace);
* likely stuck active operations, detected directly from unmatched `span_start` records
  (with the legacy child-without-parent-end heuristic retained for older logs); and
* daemon recovery log lines that report a failure, interruption, cancellation,
  or unanswered recovery probe.

The default window is 24 hours, the stuck threshold is 10 minutes, and at
most 20 incident details are printed (hard maximum 200). Counts always cover
the full selected window. `--json` emits the counts and bounded incident list
for scripts. The command is a retained-log heuristic rather than a live
health probe: retention can remove a span completion and create a false
stuck-operation candidate, and successful recovery lines are intentionally omitted.
Use the reported trace id with `prime-agent trace` for the full timeline.

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

## Optional OTLP export and derived metrics

`createOtlpSpanExporter()` in pi-ai is a fully opt-in adapter. Creating it does
not replace the JSONL reporter: attach `exporter.sink` with `addSpanSink()` and
call the returned unsubscribe function during shutdown. The adapter posts
OTLP/HTTP JSON to `<endpoint>/v1/traces` and `<endpoint>/v1/metrics` using the
built-in `fetch`; no OpenTelemetry package is required.

```ts
const exporter = createOtlpSpanExporter({
  endpoint: process.env.OTEL_EXPORTER_OTLP_ENDPOINT!,
  headers: { Authorization: `Bearer ${process.env.OTLP_TOKEN}` },
  serviceName: "prime-agent",
});
const unsubscribe = addSpanSink(exporter.sink);

// On orderly process shutdown:
unsubscribe();
await exporter.shutdown();
```

Prime Agent creates and attaches the adapter only when
`OTEL_EXPORTER_OTLP_ENDPOINT` is set. `OTEL_EXPORTER_OTLP_HEADERS` optionally
provides comma-separated `key=value` request headers. When the endpoint is
unset, the path has no timer, queue, network calls, or derived-metric work.
Export is diagnostic-only: sink and transport failures are swallowed. Orderly
CLI completion drains it for up to one second and then proceeds with exit.

The defaults batch 128 spans, retain at most 2,048 queued spans (dropping the
oldest and exposing the count through `stats()`), flush every 5 seconds, and
bound derived metrics to 256 distinct span names. Each flush exports delta
`prime_agent.span.count`, `prime_agent.span.error_count`, and
`prime_agent.span.duration_ms` sums grouped by `span.name`. `flush()` sends one
batch; `shutdown()` stops the unrefed timer and drains all queued batches.
All bounds and intervals are configurable for a host integration.

## Local log safety and retention

All local diagnostic writes pass through `appendRotatingLog`. The logs directory
is owner-only (`0700`) and log generations are owner-readable (`0600`) on POSIX.
High-confidence bearer tokens, API keys, refresh/access tokens, client secrets,
password assignments, JWTs, and common provider tokens are redacted before disk.
Rotation is serialized across processes. The active file and newest `.old` remain
plain text; older generations are gzip-compressed. Five total generations are kept
by default. Set `PRIME_AGENT_LOG_RETENTION` to an integer from 1 to 100 to change
the bound. `prime-agent trace` and `prime-agent health` read all retained generations.

Only long-running operations emit `span_start`, which makes a silent crash or hang
visible without doubling all trace traffic. Successful `extension.hooks` spans
under 25 ms are suppressed; failures and slow hooks remain visible.

## Non-goals

* No `@opentelemetry/*` runtime dependency. The OTLP/HTTP JSON adapter is built in.
* No collector is contacted unless `OTEL_EXPORTER_OTLP_ENDPOINT` is explicitly set.
* Derived metrics are process-local deltas exported through OTLP; Prime Agent does
  not embed a metrics database, dashboard server, or alerting engine.
