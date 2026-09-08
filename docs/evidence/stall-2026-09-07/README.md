# Stall evidence: 2026-09-07 Rye EventBus deadlock

This directory preserves the raw records and a bounded reproduction for the stall diagnosed from Prime Agent session data.

## Provenance classes

### Persisted facts

The following values appear directly in `session-records.jsonl`, `trace-records.jsonl`, or `orphan-journal-records.jsonl`:

- parent session: `01a0798b-5b2d-75ab-a43e-d21bce1b7251`
- child session: `01a079e8-aa23-748c-9b4a-88cb937fa10c`
- child name: `execution-primitives-full`
- trace ID: `416be8b73b78b1265c020008db687e70`
- tool call: `call_0e26efe63a1c53178b2b0cbf6abb5465`
- `tool.execute`: `f7bff4aa876e724a`
- `kernel.execute`: `76e8d2f91f4ecbf3`
- `kernel.cell`: `429381f604708259`
- `bash.command`: `cd7af9524bfe8336`
- shell PID: `1022336`
- command: `cargo fmt --all && cargo test -p rye-agent-core --test execution_primitives`
- command duration: `81,803,544.573 ms`
- termination: exit `-15`, signal `SIGTERM`
- exact hanging test output ends at `event_bus_continues_watcher_delivery_after_listener_failure_and_reports_it ...`

The session tool-result timestamp and all deferred span ends were written only after cleanup terminated the stale process group. Before termination, Python had not persisted `kernel.cell` or `bash.command` starts. This missing in-flight evidence motivated commit `10a5f2aaf`.

### Deterministic derivations

The Bash runtime starts every command with `start_new_session=True` and later signals it with `os.killpg(process.pid, ...)`. On Linux this makes the shell PID the new process-group ID. Therefore the persisted PID `1022336` was also PGID `1022336`. The historical `/proc/1022336/stat` record no longer exists after cleanup, so this is a runtime-contract derivation rather than a surviving kernel record.

The full W3C traceparent was not persisted. Its recoverable prefix is:

```text
00-416be8b73b78b1265c020008db687e70-cd7af9524bfe8336-??
```

The trace ID and parent span are persisted. The final two-character flags byte was never logged and cannot be recovered honestly.


## Identifier provenance ledger

`identifier-provenance.json` classifies every relevant identifier as directly persisted, independently reproduced, deterministically derived, or unrecoverable. In particular:

- the original shell PID is persisted;
- PGID equality is derived from the runtime process-group contract because the original `/proc` entry is gone;
- the isolated reproduction independently records PID = PGID = SID and both thread wait channels;
- the W3C flags byte and a pre-termination Bash start/end record were never persisted and are explicitly `null`.

## Mutex cycle

The historical implementation represented by `historical-eventbus-deadlock.rs` preserves the load-bearing cycle:

1. `emit_batch` acquires the non-reentrant delivery mutex.
2. It invokes a user-controlled watcher while retaining that mutex.
3. The watcher reports `Err("watcher failed")`.
4. The synchronous error path calls public `emit("handler_error")`.
5. Nested `emit_batch` attempts to acquire the same delivery mutex.
6. The outer call cannot release it until the error callback returns.

This mirrors the historical Rye implementation and the exact watcher-failure test created by the persisted tool call.

## Reproduction

Run:

```bash
./docs/evidence/stall-2026-09-07/reproduce.sh
```

The script compiles outside both execution deadlines. It then:

1. runs the historical cycle under an 8-second timeout;
2. records exit `124`;
3. records both threads in `futex_do_wait`;
4. runs the exact current Rye regression under the same 8-second deadline;
5. records exit `0` and one passing test.

Expected checked-in evidence:

- `historical-exit-code.txt`: `124`
- `historical-process-state.txt`: main and `event_bus_conti` threads in `futex_do_wait`
- `current-control-exit-code.txt`: `0`
- `current-control.stdout`: exact regression passes

Process IDs in `historical-process-state.txt` are reproduction-local and change on each run.
