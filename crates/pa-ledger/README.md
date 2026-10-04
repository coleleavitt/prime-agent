# pa-ledger

The failure ledger and the resolution index. Runtime failures observed in sessions (Python tracebacks in tool output,
tool results flagged `isError`, assistant replies that ended with `stopReason: "error"`) are fingerprinted, counted in a
ledger kept in the harness state (per session, and per machine unless `PRIME_AGENT_GLOBAL_LEDGER=0|off|false|no`), and
the failed `tool.execute` span carries the fingerprint as `failure.fingerprint`. A Python failure is joined to the
`ipython` cell that later stopped it, and when the same failure recurs the `ipython` result carries that cell back to
the model in an `<ipython_resolution_hint>` block.

Behavioural spec: the TS product's `packages/coding-agent/src/core/ravo/failure-ledger.ts`, the data half of
`ravo/referee.ts` (replay cases), `distill/resolution-index.ts`, and the ledger wiring in `agent-session.ts`
(`_observeFailuresAtTurnBoundary`, `_flushFailureLedger`, `_flushGlobalFailureLedger`,
`_tagFailureFingerprintOnCurrentSpan`) on `perf/session-catalog-resume`.

## Scope

- Fingerprints: message normalization (lowercase; paths `<path>`, quoted strings `?`, hex ids `<hex>`, numbers `#`;
  whitespace collapsed; 200 UTF-16 units), `sha256(canonicalJson({kind, source, exceptionClass, message}))[:16]`,
  the last-traceback parser and skill attribution, `failure:<id>` opponent ids.
- The ledger: observation, update and cross-session merge, the strict-majority actionability tally (aborts, denials, a
  dead kernel, network/DNS, timeouts, a locked database; provider capacity, 5xx, refusal, content filter, empty
  completions), the recurring set and newly-recurring detection, the durable observation ordinal, the prompt block
  and the recurrence/regression refine instructions, provisional-regression detection and recording over a stored RAVO
  state, replay-case storage (derive, validate, merge, verify).
- The resolution index (window 6 cells, 64 records, 1 200-unit cells, a shared identifier required) and its per-repo
  durable store.
- The session feature: turn-boundary observation on the feature's own worker thread, local and global flushes (the
  global one under the TS harness-state lock), the span annotation, the `ipython` hint.

## Non-goals

- RAVO itself (`pa-ravo`): the refine gate, queuing recurrence/regression refines, the referee that runs replay cases,
  trust windows. They plug in through `LedgerObserver` and `LedgerHandle` (below).
- No CLI command: the TS product had none for the ledger or the index.
- No daemon wire event, no TUI surface.

## Seams

- `pa_core::features::SessionFeature` (installed by `pa-cli` behind `feature = "ledger"`, on by default):
  - `on_session_start`: the resumed history sets the branch index and the assistant-turn count;
  - `on_message_end`: every message is fingerprinted in memory; each assistant message is a turn boundary (TS
    `message_end`), whose observations, stamped with the boundary's turn and time, are folded into the ledgers on the
    worker. Sessions without an artifact dir observe nothing (TS: no local harness state dir);
  - `after_tool_call`: a failed result's fingerprint is recorded on the open `tool.execute` span; an `ipython` cell
    that ran (status `ok`/`error`/`aborted`, not a crashed kernel) feeds the resolution index on the blocking pool, and
    a recurrence returns the hint text, appended to the result;
  - `on_agent_end`: queues a flush (TS flushes at `agent_end`);
  - `flush`: flushes every session and waits for the worker until the exit deadline.
- `tracing`: an event under `pa_types::trace_context::SPAN_ATTRIBUTES_TARGET` (`failure.fingerprint = <id>`), merged
  into the span's attrs by `pa-trace`; the span `harness.ledger.flush` (`session.id`, `ledger.scope`,
  `ledger.observations`, `ledger.verifications`, `ledger.fingerprints`, `error` on failure) around each global flush.

## Files owned

All byte-compatible with the TS product (proved by `tests/golden.rs` against outputs of the TS sources run under node;
`tests/fixtures/golden/generate.ts` regenerates them):

- the `failures` key of `<sessionArtifactDir>/harness/harness_state.json` (local: the session's counts and its scan
  cursor) and of `<agentDir>/harness/harness_state.json` (global: counts across sessions; the cursor stays 0). The
  document is read and written as raw JSON: every other key is carried through untouched (in place, in order), a new
  `failures` key lands where a TS save puts it (after `ravo`, before `trustWindows`), a missing or corrupt file starts
  from the TS empty state. `JSON.stringify(state, null, 2) + "\n"`, temp file + rename, the file's mode kept (0600 for
  a new one). The global read-modify-write holds the TS `proper-lockfile` lock (`harness_state.json.lock`, 10 s stale,
  200 attempts 5 ms apart);
- `<agentDir>/resolution/<basename>.<sha256(repoDir)[:16]>.json`: `{version: 1, repo, records}` for the git worktree
  holding the session's cwd (none outside one), at most 64 records, mode 0600 in a 0700 directory, under
  `<store>.lock` (40 attempts).

Known, deliberate differences, each confined to inputs the TS product cannot round-trip either: a 200/400/1 200-unit
cut through a surrogate pair drops the pair (JS keeps a lone surrogate); a record holding both `replayCases` and
`nonActionableCount` always writes them in that order (TS writes the reverse right after a replay merge); a tool result
whose stored `toolName` is missing fingerprints with source `""` (Rust cannot tell missing from empty).

## Telemetry

`failure_resolution_hint` (schema v4): `origin` (`session` | `store`) when a hint is appended. Never the fingerprint,
the cell source, the exception class, or a path.

## Public API for `pa-ravo`

Pure (no I/O): `FailureFingerprint`, `FailureKind`, `fingerprint_failure`, `fingerprint_tool_result_text`,
`normalize_failure_message`, `parse_python_traceback`, `failure_opponent_id`, `FAILURE_OPPONENT_PREFIX`;
`FailureLedger`, `FailureRecord` (`is_actionable`, `non_actionable_occurrences`, `verified_replay_cases`),
`FailureObservation` (`is_actionable`), `LedgerUpdate`, `update_failure_ledger`, `merge_failure_observations`,
`normalize_failure_ledger`, `recurring_failures`, `observation_ordinal`, `apply_replay_verifications`,
`ReplayVerification`, `format_failure_ledger_for_prompt`, `format_recurrence_refine_instructions`,
`format_regression_refine_instructions`, `ProvisionalRegression`, `find_provisional_regressions` /
`record_provisional_regressions` (over the stored `ravo` JSON value), `ReplayCase`, `ReplayProbe`,
`derive_replay_case`, `replay_probe_of`, `replay_probe_source`, `merge_replay_case`, `normalize_replay_cases`,
`verified_replay_cases`, `is_replayable_module_path`, `REPLAY_MODULE_DENYLIST`, `MAX_REPLAY_CASES`,
`extract_failures`, `observe_message`.

Harness state: `HarnessDocument` (`load`, `get`, `set`, `failures`, `set_failures`, `save`),
`with_harness_state_lock`, `global_harness_state_dir`, `local_harness_state_dir`,
`global_failure_ledger_enabled[_from_env]`.

Runtime: `FailureLedgerFeature::with_observers(options, observers)` and `.handle()`. A `LedgerObserver` sees every
`LedgerBoundary` (turn, observations, local and effective ledgers, newly recurring records, actionably recurred ids,
local and global ordinals, the global document read for it), may hold a session's flushes (`hold_flush`, e.g. while a
refine plan binds the state), may ask for a local or a global flush with nothing observed (`wants_local_flush`,
`wants_global_flush`), writes its own
keys into the same document inside the flush (`on_flush`, under the lock for the global scope), and learns whether it
landed (`on_flush_result`). `LedgerHandle`: `session_ledger`, `fresh_global_ledger`, `turn`,
`record_replay_verifications`, `request_flush`, `request_global_flush` (the global state only: for a caller that
may run while the kernel writes the local state), `wait_idle`, `global_ledger_enabled`, `local_harness_state_dir`.
