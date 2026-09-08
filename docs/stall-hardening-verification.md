# Stall observability and coverage hardening verification

Verified on 2026-09-08. This record maps the requested work to committed source, regression tests, and executed gates.

## Implementation matrix

| Requirement | Implementation | Regression evidence |
| --- | --- | --- |
| Python in-flight span visibility | `prime-agent-runtime/src/rlm/trace.py`, `bash.py`; host forwarding in `repl-manager.ts` | `test_trace.py`, `test_trace_bash_mcp.py`, `repl-kernel-trace.test.ts` |
| Bash progress, silence, and Cargo lock diagnosis | Active Bash snapshots and `command_progress`, `command_no_output`, `cargo_lock_wait` in `bash.py` | Python Bash, trace, and REPL suites |
| Fatal process handlers | Visible stderr plus structured crash records in `process-crash.ts` | real subprocess fixture in `process-crash.test.ts` |
| Concurrent log rotation | bounded cross-process lock retry in `config.ts` | six-process rotation fixture in `local-log.test.ts` |
| OTLP lifecycle | runtime install, signal drain, bounded hung collector handling | `otlp-export-process.test.ts` with local HTTP collector |
| Bounded child lifecycle | phase deadlines and `child.passivate`/`child.delete` spans in `daemon-mode.ts`; bounded host deletion in `agent-session.ts` | `daemon-mode.test.ts` timeout, sibling-progress, cleanup, and retry cases |
| Orphan journal integrity | fail-closed parsing plus structured append/read/reap outcomes | `orphan-process-journal.test.ts`, supervisor monitor tests |
| Kernel bootstrap contention | owner token, PID start identity, progress, stale recovery, 120-second bound, `kernel.bootstrap_lock` span | `kernel-bootstrap.test.ts` |
| Rye recursive event publication | queued FIFO drain outside callback mutex in `harness/events.rs` | ordinary listener, watcher, filter, and error-listener nested publication tests |
| Rye subprocess cleanup | `ChildGuard`, process-group termination, bounded output collection | CLI/RPC smoke and process module tests |
| Bounded Rust tests | nextest CI profile, per-job timeouts, bounded deadlock regressions | `.config/nextest.toml`, Rye CI |
| Prime health | process, kernel, child, lock, orphan, diagnostic categories; stale/empty/malformed evidence is `unknown`; incidents exit 2 | `health-command.test.ts` |
| Multi-runtime coverage | Vitest V8, c8, coverage.py, cargo-llvm-cov; one merged Prime upload | `docs/coverage.md`, both repositories' workflows |
| Fail-closed Codecov | project, patch, and critical component statuses; upload failures block CI | both `codecov.yml` files and workflow gates |

## Incident evidence

`docs/evidence/stall-2026-09-07/` contains raw session, trace, and orphan-journal records plus a bounded historical/control reproduction. Commit `5af42a109` documents provenance limits rather than inventing the unpersisted trace-flags byte.

The historical reproduction exits 124 after eight seconds with both threads in `futex_do_wait`. The exact current regression exits 0.

## Commits

Prime Agent:

- `10a5f2aaf` — functional observability, lifecycle bounds, process diagnostics, and integration regressions
- `ccfcb1448` — JS/Python coverage and fail-closed Codecov
- `5af42a109` — durable incident evidence

Rye:

- `9b8be86` — queued event delivery, bounded subprocesses, nextest, llvm-cov, and Codecov
- `42f9417` — load-stable timeout output regression

## Fresh verification from committed state

Prime Agent:

- `npm run check`: passed; Biome checked 1,021 files with no fixes, TypeScript passed, installer passed, browser smoke passed.
- Combined focused hardening suite: 11 files, 406 tests passed.
- Python runtime discovery: 328 tests passed.
- Earlier coverage execution after the coverage commit: coding-agent default 4,979; process 30; kernel 15; agent 81; AI 371 with 731 credentialed tests skipped in a clean environment; TUI 766.

Rye:

- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `cargo nextest run --workspace --all-targets --all-features --profile ci`: 1,024 passed, 4 skipped.
- workspace doctests: passed.
- timeout-output regression: passed three concurrent runs before the full gate.
- earlier `cargo llvm-cov` execution: 87.66% line coverage.
- historical/control reproduction: passed its exit-code assertions.

## Repository ownership and cleanliness

Prime Agent has no uncommitted tracked changes. Existing unrelated untracked `.cortexkit/`, `.pi/`, release, and session HTML artifacts were not staged.

Rye had no tracked baseline when this work began: the entire project was pre-existing and untracked. The two Rye commits contain only the explicitly changed hardening and regression files. The remaining baseline files stay untracked and untouched; they were not swept into these commits. Consequently the Rye patch checkpoints are coherent but are not a standalone repository snapshot until its owner creates a baseline commit.
