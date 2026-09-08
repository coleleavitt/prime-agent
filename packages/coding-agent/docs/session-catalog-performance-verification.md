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
| 100 | 9.79 ms |
| 200 | 14.11 ms |
| 400 | 27.15 ms |
| 800 | 51.07 ms |

The sample covers reconciliation, index construction, filtering, rollups, row construction, and render preparation. It is evidence of the expected near-linear trend, not a timing-gated test.

## Deterministic tests

Modified performance/regression files: **50/50 tests passed** across 5 files.

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
