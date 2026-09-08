# Session Catalog Performance Verification

Verified on commit `829424abe7119fbc987ffed4ca8d7d629cb6566a`.

## Committed checkpoints

- `9c951bd4221089a8910dee180e86c6546b5d3e5a` — made dashboard catalog loading near-linear.
- `6bcaf4b1f2aae7d6d0c7662ae61efc2e61abb8a2` — reduced selector resume from two flat-catalog scans to one.
- `829424abe7119fbc987ffed4ca8d7d629cb6566a` — asserted constant reconciliation count for 1 and 100 saved sessions.

## Source invariants

- `refreshSavedSessions` has no per-session progress callback.
- Its successful terminal path calls `reconcileCatalogs()` once.
- `resolveSessionPath` calls `SessionManager.list()` zero times and `SessionManager.listAll()` once.
- The existing daemon command, progress event, and final response shapes are unchanged; no schema revision or capability is required.
- Rich transcript search remains. The query is compiled once per rebuild and each corpus is normalized once per match.
- Active-daemon client parsing remains where local extension shortcuts require a real client-side `SessionManager`; removing it safely requires a daemon-backed proxy or negotiated snapshot.

## Scaling sample

Current real catalog: 156 sessions.

| Synthetic sessions | Integrated build and render preparation |
| ---: | ---: |
| 100 | 7.34 ms |
| 200 | 13.06 ms |
| 400 | 28.05 ms |
| 800 | 60.94 ms |

Each value is the median of five current-HEAD runs. The sample covers reconciliation, index construction, filtering, rollups, row construction, and render preparation. It is evidence of the expected near-linear trend, not a timing-gated test.

## Deterministic tests

Modified performance/regression files: **51/51 tests passed** across 5 files.

- `test/agents-view-mode.test.ts`
- `test/agents-view-usage-layout.test.ts`
- `test/session-manager-list.test.ts`
- `test/session-view-search.test.ts`
- `test/suite/regressions/4722-invalid-resume-selector.test.ts`

Daemon compatibility: **137/137 tests passed** across:

- `test/saved-session-catalog.test.ts`
- `test/daemon-client.test.ts`
- `test/agent-connection-daemon.test.ts`

## Repository gate

`npm run check` passed:

- Biome checked 1,025 files with no fixes.
- TypeScript checking passed.
- Installer check passed.
- Browser smoke check passed.

## Git-derived range verification

The exact range from the parent of the first performance commit through the verification commit also contains unrelated concurrent changes. Git identified 10 changed test files across `packages/ai` and `packages/coding-agent`.

With the agent-only `RLM_MAX_DEPTH` override removed, all Git-derived changed tests passed:

- `packages/ai`: **24/24 tests passed** across 1 file.
- `packages/coding-agent`: **225/225 tests passed** across 9 files.

This broader run includes the five performance/resume test files listed above and every unrelated test file changed by interleaved commits in the same history range.

## Independent final audit

An independent code audit of commits `9c951bd4`, `6bcaf4b1`, and `829424ab` reported no release-blocking production findings. A follow-up test audit found that the reconciliation regression needed to emit progressive wire events; the test now emits every `session_list_item` before the terminal response. It reviewed the complete changed implementations and surrounding call sites for reconciliation complexity, scanner bounds and order, search parity, render-cache expiry, resume matching semantics, daemon protocol classification, test adequacy, and changelog format.

A follow-up audit also found clock-rollback invalidation missing from the render cache. The cache now invalidates when observed wall time moves backward, with a regression test. Remaining non-blocking risks are pathological reverse-ordered deep hierarchies, filesystem-scale stress beyond the deterministic scanner tests, lexical rather than realpath cwd equivalence, and possible future drift if the upstream fuzzy scorer changes.
