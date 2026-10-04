# pa-learning

The continual-learning gate (fork feature crate, `docs/fork-feature-crates.md`): the learning index rolls the
structured log up into sealed days keyed by failure fingerprint, `prime-agent learning` measures whether the
fingerprints refinements claimed to address got rarer than the rest, and the Engineer Trajectory Index (ETI) labels
each fingerprint's course over ISO-week windows and feeds the stable residue back into the harness digest and the
recurrence reminders.

Behavioural spec: the TS product's `packages/coding-agent/src/core/learning-index.ts`, `src/cli/learning-command.ts`,
`src/cli/learning-chart.ts`, `src/core/distill/trajectory-index.ts`, the trajectory half of `agent-session.ts`
(`_harnessTrajectoryBias`, `_trajectoryInternalizedReminders`) and of `formatHarnessStateForPrompt`, and their tests
(`test/learning-index.test.ts`, `test/trajectory-*.test.ts`, `test/suite/regressions/eti-trajectory-prompt.test.ts`)
on `perf/session-catalog-resume` (key commits `5fdd7f6dc`, `24bf91607`, `4c99bf8c2`).

## Scope

- **Learning index**: the retained log generations (`agent.jsonl.old.<n>.gz`, `.old`, live; gzip bounded at 64 MiB)
  rolled up per UTC day: per key `count`, `p50Ms`/`p95Ms` (20 000 samples, then `sampled`), the normalized error
  text; `agent.turn` span ends are the turns; a `refinement.committed` line that claims at least one fingerprint is a
  commit. A span's key is its `failure.fingerprint` attribute (the ledger id), else
  `span:<sha256(name NUL status NUL message)[..16]>`. Sealing writes every complete day (the current UTC day stays
  open) once; days are re-read leniently (claimless commits of older files dropped).
- **Report**: the median commit is the pivot, its day excluded from both windows; per-fingerprint failure rates per
  1000 turns before and after, the treated (claimed) and control cohorts, the daily cohort series, and a one-sided
  Mann-Whitney U (midranks, tie correction, continuity correction, A&S erf with V8's `Math.exp`), withheld below the
  minimum cohort size (default 5), without exposure on a side, or without variance.
- **`prime-agent learning [--log <path>] [--index <dir>] [--min-n <n>] [--limit <n>] [--no-seal] [--no-chart]
  [--json]`**: seal, then print the table, the cohorts, the significance (or why it is withheld) and the ASCII chart;
  exit 0 with a p-value, 2 when withheld, 1 on a usage or read error.
- **ETI**: sealed days bucketed into observed ISO weeks (week-year, Monday start; the current week open), per window
  per failure fingerprint counts and the running ordinal; labels NEW (first seen in the newest window), DROPPED
  (recurred, then absent for the last `--gap` windows, unless a commit claimed it or the domain went inactive),
  PERSISTS (recurring in a strict majority of windows), all withheld below `--min-windows` (default 4); every label
  carries `confounds` (`task-mix`, plus `tool-surface` / `measurement-instrument` for backfill corpora, a mixed index,
  or a span crossing an observed-week gap); the appeared/retired rate (`null` across a gap); a security-class
  predicate (`auth|credential|secret|token|password|permission[- ]denied|unauthor`).
- **`prime-agent learning trajectory [--index <dir>] [--include-backfill] [--min-windows <n>] [--gap <m>] [--no-seal]
  [--limit <n>] [--json]`**: seal the index, compute and store the prime-only trajectory (in a `trajectory.seal` span:
  `windows`, `labelled`, `withheld`, `backfill`), print the table and the rate; `--include-backfill` folds the offline
  cross-tool day files (`skills/trajectory-backfill/backfill.py`) into the printed table only. Exit 2 when every prime
  label is withheld.
- **Lever 1 (digest)**: entries joined to a labelled fingerprint through the state's `trustWindows` (and the ledger's
  `addressedByProposalIds`) rank stable gaps first, then new, then the unlabelled, then internalized ones (security
  classes never internalized); up to three confound-flagged stable-gap lines render under `engineer trajectory
  (confound-flagged; local signal, may reflect task-mix):`. Explicit ledger fingerprints only.
- **Lever 2 (reminders)**: a DROPPED fingerprint queues no recurrence refine, unless it is a security class or recurs
  live in the session's own ledger. Kill switch for both levers: `PRIME_AGENT_TRAJECTORY_INDEX=0|off|false|no`.

## Non-goals

- In-session Dream (phase 4, `pa-dream`), trust windows themselves (opening, settling; `pa-ravo`'s next phase): the
  join reads `trustWindows` wherever a producer stored them.
- The resolution of the known double count (an in-cell traceback counted on `kernel.cell` and on the enclosing
  `tool.execute`), deliberately left as in TS.
- Running `backfill.py`: it is a standalone offline script; this crate only reads its output.

## Seams

- `pa_core::features::SessionFeature::harness_prompt_hook` (installed by `pa-cli` behind `feature = "learning"`):
  `TrajectoryPromptHook` answers the digest's `HarnessPromptAdjustment` (entry ranks, the trajectory section) from
  the sealed store, per render, through a `(mtime, size)` stat cache: no span, no day scan, no network.
- `pa_ravo::RecurrenceFilter` (`LearningFeature::recurrence_filter`, attached by `pa-cli` with
  `RavoFeature::attach_recurrence_filter`): Lever 2.
- `pa-cli`'s command registry: `learning` / `learning trajectory` dispatch into `command::run_learning_command`.
- The structured log `pa-trace` writes (`pa_trace::retained_log_files` / `read_log_text`): `span_end` records,
  `failure.fingerprint` span attributes stamped by `pa-ledger`, and `pa-ravo`'s `refinement.committed` events.

## Files owned

Byte-compatible with the TS product (proved by `tests/golden.rs` against outputs of the TS sources run under node;
`tests/fixtures/golden/generate.ts` regenerates them):

- `<agentDir>/learning/days/<YYYY-MM-DD>.json`: one sealed day, `JSON.stringify(day, null, 2) + "\n"`, temp file +
  rename, `0600` in a `0700` directory.
- `<agentDir>/learning/trajectory.json`: the prime-only, newest-52-windows trajectory, temp file + fsync + rename under
  the `proper-lockfile` lock `trajectory.json.lock` (40 attempts 5 ms apart, 10 s stale), `0600`; validated on read
  (version 1, at most 8 MiB, every label with a known kind and non-empty confounds), else treated as absent.
- `<agentDir>/learning/.gitignore` (`trajectory.json`, `backfill/` appended when missing).
- Read only: `<agentDir>/learning/backfill/<corpus>/<YYYY-MM-DD>.json` (the backfill script's output).

Known, deliberate differences: `refinement.committed` lines are read in the TS shape (`proposalId`, `addressed`
array) and in the shape `pa-ravo` records through `tracing` (`proposal_id`, `addressed` joined with commas).
`localeCompare` is ICU's root order for printable ASCII (the identifiers sorted here). A store temp file's 4-byte
suffix is derived from the clock and thread instead of `randomBytes` (the lock serializes writers). I/O error texts
are Rust's. As in TS, the digest renders the merged harness state, which carries no `trustWindows`, so Lever 1's entry
ranking only fires for a state that stores them; the stable-gap lines always render.

## Telemetry

`learning_report` (schema v4, emitted by `pa-cli` per parsed invocation): `subcommand` (`report` | `trajectory`),
`outcome` (`reported` | `withheld` | `failed`), `days` (sealed days read), `sealed` (days this run sealed), `backfill`,
`duration_ms`. Never a fingerprint, a path, a rate or a p-value.

## Public API

`LearningFeature` (`recurrence_filter`); `command` (`run_learning_command`, `LearningCommandIo`,
`LearningCommandOutcome`, `LearningRunReport`, `LearningSubcommand`, `LearningOutcome`, the help constants,
`format_learning_report`, `format_significance`, `learning_chart_lines`, `build_trajectory_report`,
`format_trajectory_report`); the index (`roll_up_learning_days`, `seal_learning_days`, `read_learning_index`,
`write_learning_day`, `normalize_day`, `span_fingerprint_key`, `LearningDay`, `FingerprintDayStats`,
`RefinementCommit`); the report (`build_learning_report`, `mann_whitney_one_sided`, `normal_cdf`, `LearningReport`);
the chart (`render_ascii_chart`); the trajectory (`seal_trajectory_windows`, `iso_week`, `matches_security_class`,
`trajectory_index_enabled`, `TrajectoryStoreFile`, `read_trajectory_index`, `write_trajectory_index`,
`read_backfill_days`, the path helpers); the levers (`trajectory_prompt_adjustment`, `trajectory_class_for_entries`,
`format_trajectory_lines`, `trajectory_internalized_fingerprints`, `TrajectoryPromptHook`, `InternalizedReminders`).
