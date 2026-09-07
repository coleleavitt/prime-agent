# Observability hardening verification

Verified: 2026-09-07

This record maps the requested observability work to committed implementation and test evidence. The implementation commits are `272cd6e23` and `d2eb1a022` in Prime Agent, plus `69d06746` in Magic Context.

## Requirement matrix

| # | Requirement | Committed implementation | Focused evidence |
|---|-------------|--------------------------|------------------|
| 1 | Centralized redaction and owner-only log permissions | `packages/coding-agent/src/config.ts`: `redactLocalLog`, secure directory/file modes | `packages/coding-agent/test/local-log.test.ts` |
| 2 | Cross-process-safe rotation | `packages/coding-agent/src/config.ts`: stable sidecar lock around prepare, rotate, and append | `packages/coding-agent/test/local-log.test.ts`; full type/lint checks |
| 3 | Bounded retention and compression | `PRIME_AGENT_LOG_RETENTION`, `.old` compatibility, gzip generations; retained generations read by `trace` and `health` | `local-log.test.ts`, `trace-command.test.ts`, `health-command.test.ts` |
| 4 | Span start or active-operation visibility | allowlisted `span_start` records in `packages/ai/src/trace-context.ts`; open-span reconstruction in trace and health commands | `packages/ai/test/trace-context.test.ts`, `trace-command.test.ts`, `health-command.test.ts` |
| 5 | Structured crash and kernel-exit diagnostics | `packages/coding-agent/src/core/process-crash.ts`, `cli-main.ts`, `core/kernel/repl-manager.ts` | `process-crash.test.ts`, `repl-kernel-startup.test.ts` |
| 6 | Reduce high-volume extension hook spans | `ExtensionRunnerOptions.traceHookMinDurationMs`, default 25 ms; errors and slow hooks retained | `extensions-runner-trace.test.ts` |
| 7 | Optional OTLP export and derived metrics | dependency-free OTLP/HTTP adapter, additive sink, bounds, timeouts, abortable/idempotent shutdown, standard env wiring | `packages/ai/test/otlp-span-exporter.test.ts`, `packages/coding-agent/test/otlp-export.test.ts` |
| 8 | Health summaries | `prime-agent health` reports historian failures, provider failures, locally observed stuck prompts/turns, and daemon recovery failures | `packages/coding-agent/test/health-command.test.ts` |

## Bugs found by retained-trace analysis

- Disabled automatic trace upload emitted 4,065 `trace.upload` spans. The enabled preflight now runs before opening a span (`packages/coding-agent/src/core/agent-traces.ts`).
- `extension.hooks` accounted for about 45% of retained spans. Fast successful hook spans are now suppressed while errors, slow hooks, and child parenting remain observable.
- One Magic Context session emitted 230 no-op historian runs, usually about 11 seconds apart, after its protected-tail drain budget was exhausted. Magic Context commit `69d06746` latches the durable earliest retry time, preserves the 60-second emergency failure backoff, and fences late callbacks by activation generation.
- Traces with up to 24,272 spans were investigated. Each had one `client.prompt` root and represented a legitimate long-running agent prompt, not ambient trace-context leakage.

## Passing verification

Run after the implementation commits:

- `npm run check`: passed. Biome checked 1,017 files with no fixes; TypeScript, installer, and browser smoke checks passed.
- Coding Agent focused observability suite: 15 files, 188 tests passed.
- AI trace and OTLP suite: 2 files, 29 tests passed.
- Python trace suite: 42 tests passed.
- Magic Context Pi context/historian suite: 146 tests passed and Pi plugin typecheck passed.
- Magic Context drain limiter/emergency suite: 20 tests passed.

## Repository state

The implementation was committed as coherent checkpoints and not pushed. At verification time Prime Agent was ahead of its tracked remote by two implementation commits, and Magic Context was ahead of `upstream/master` with `69d06746` included. Existing unrelated untracked artifacts in Prime Agent were not staged or modified.
