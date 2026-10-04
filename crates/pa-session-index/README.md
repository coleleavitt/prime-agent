# pa-session-index

The persisted saved-session catalog index. The native catalog scan folds every session file in every fresh process,
so a restarted daemon's first listing reparses the whole store. This crate persists what each file listed as, keyed by
the file's `(size, mtimeMs)` (session files are append-only, so an unchanged key means unchanged content), and serves
those rows to the next process without the fold.

Behavioural spec: the TS product's `packages/coding-agent/src/core/session-catalog-index.ts` and its use in
`session-manager.ts` (`primeSessionInfoCacheFromIndex`, `persistSessionInfoCache`) on `perf/session-catalog-resume`
(commits `9c951bd42`, `6bcaf4b1f`, `35f23b41b`, `566158ace`, `4fbdcfe9d`, `829424abe`).

Measured (release, page cache warm, fresh process, `pa_daemon::session_store::list_sessions`):

| Store | Without the index | Fresh process, index present | Index files |
| --- | ---: | ---: | ---: |
| 2,000 synthetic sessions, 3.6 GB | 2.15-2.33 s | 89-112 ms | 3.7 MB + 40 MB |
| 373 real sessions, 783 MB | 0.88-0.89 s | 18-19 ms | 0.9 MB + 6.4 MB |

The rows are identical with and without the index (whole-row equality over both stores).

## Scope

- Read a session directory's two tiers once per process (on that directory's first lookup), serve rows whose key and
  fold version match, record what a miss folded to, drop files that left the listing, and rewrite both tiers only
  after a listing learned something, on a writer thread (`<file>.<pid>.tmp`, then rename; owner-only).
- `info: null` lines: a file that is not a session is not rescanned until it changes.

## Non-goals

- The TS product's deferred corpus (`deferred_session_search_text`, `includeSearchText`): the native wire always
  carries `allMessagesText`, so a row is served only with its corpus line, and the corpus tier keeps every listed
  session (TS keeps the newest 200).
- Serving TS-written lines: their fold differs from the native one (TS re-serializes `created`, its usage rules
  predate the native ones), so a line without `foldVersion` is never served; the file is folded once and its line
  rewritten in this crate's form.
- Agents-view render caching and the other TS dashboard optimizations of the same commits: the native catalog
  already streams rows during the scan.

## Public API

- `SessionIndex` (`new`, `flush(deadline)`), implementing `pa_core::session::catalog_cache::SessionCatalogCache`.
- `SESSION_INDEX_FILE`, `SESSION_SEARCH_INDEX_FILE`.

## Seams

- `pa_core::session::catalog_cache` (installed by `pa-cli` behind `feature = "session-index"`, on by default): the
  native catalog scan (`pa_daemon::session_store::list_sessions`, which serves `list_saved_sessions`, the saved-session
  resolve, and the most-recent-session lookup) asks `lookup` per file on its blocking-pool thread, `record`s a miss
  whose key held across the fold, and calls `scan_finished` after a complete listing. `pa-cli` drains pending writes at
  exit (bounded, `InstalledFeatures::finish`).

## Files owned

Both in the session directory, line-compatible with the TS product (`tests/fixtures/ts-*.ndjson` were written by the
TS writer, see `tests/fixtures/generate.ts`; this crate's output equals them byte for byte apart from its additions):

- `session-index.ndjson`: `{"version":2}`, then `{"file","size","mtimeMs","info"}` per file (`info` in the TS
  `SerializedSessionInfo` key order, `null` for a non-session file).
- `session-search-index.ndjson`: `{"version":2}`, then `{"file","size","mtimeMs","searchText"}` per session.

Additions the TS reader ignores: `foldVersion` at the end of every line (the native fold's version the row was derived
under) and `info.thinkingLevel` (after `modified`). Whole numbers print as `JSON.stringify` prints them; numbers are
parsed from their text exactly.

## Telemetry

None: the index changes no user-visible surface (the listing's rows and wire frames are identical), so there is no
adoption to measure. `tracing` debug events under target `pa_session_index` report load and write sizes and times.
