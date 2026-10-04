# pa-recall

Workspace Recall: the fork feature that stores the *invalidation*, not the answer. When a top-level session's agent
run ends, it writes a per-repo mark of digests (HEAD, the index, every dirty path, the names of skip-worktree paths a
sparse checkout left off disk, and up to 8 build claims). The first `ipython` result of the next top-level session in
that repo recomputes every digest against the live filesystem and gets a `<workspace_recall>` block of at most 2 KB
appended: what changed, how many paths are provably unchanged, what could not be compared, and whether each build
claim is still CURRENT or EXPIRED (with a reason).

A build claim is a `bash()` build or test command (`tsc`, `tsgo`, `biome check`, `cargo test|build|check|clippy`,
`npm|pnpm|yarn|bun test|run check`, `pytest`, `go test`, `make`, ...) that exited 0 inside an `ipython` cell while the
workspace digest held still. A claim is never served as an answer; it is CURRENT only while the workspace digest it
was recorded against still matches.

Behavioural spec: the TS product's `packages/coding-agent/src/core/recall/` and
`core/extensions/builtin/workspace-recall.ts` on `perf/session-catalog-resume`.

## Scope

- Workspace capture via `git` (`rev-parse`, `ls-files -s -v -z`, `status --porcelain=v1 -z`, `diff --name-only`,
  `ls-tree`, `hash-object`), file hashing (sha256 truncated to 128 bits, `sha256-128`), 8 MiB per file and 64 MiB per
  capture, at most 200 recorded dirty paths, skip-worktree / assume-unchanged handling.
- The mark store, the shared git-timeout skip entry, the witness comparison, the block renderer.
- The session feature: build-cell digests before/after an `ipython` cell, the first-result witness, the run-end mark
  on a background worker, deadlines and skip windows.

## Non-goals

- No cached answers: no file content and no command output is ever stored.
- RLM child sessions (`rlm_depth > 0`) never mark and never get a block.
- No CLI command, no daemon wire event, no TUI surface.

## Seams

- `pa_core::features::SessionFeature` (installed by `pa-cli` behind `feature = "recall"`, on by default):
  - `before_tool_call`: an `ipython` cell whose source mentions a build command gets a workspace digest (1 s deadline);
  - `after_tool_call`: claims from the cell's `host_facts.bashCommands` (exit code 0, not truncated, a build command,
    the digest unchanged); on the session's first `ipython` result (no earlier one in context), the witness and the
    appended block (1 s deadline);
  - `on_agent_end`: schedules the mark on the crate's own one-thread runtime (reruns coalesce per session);
  - `flush`: `pa-cli` waits up to 2 s at process exit for pending marks.
- `tracing` spans `recall.mark` (a detached root), `recall.witness`, `recall.digest`, with the TS span attributes
  (`recall.repo_key`, `recall.skipped`, `recall.skip_reason`, `recall.negative_cache`, `recall.changed`, ...).

## Files owned

Under `<agentDir>/recall/` (`~/.prime/agent/recall/`), byte-compatible with the TS product so both binaries share them:

- `<basename>.<sha256(repoRoot)[:16]>.json`: the mark (`schema: 1`, `digestAlgorithm: "sha256-128"`), written
  `JSON.stringify(mark, null, 2) + "\n"`, mode 0600, temp file + rename, under the `proper-lockfile`-compatible
  `<mark>.lock` directory;
- `<basename>.<hash>.skip.json`: `{schema: 1, reason: "git_timeout", until}`; every process sharing the agent dir
  leaves the repo alone for 10 minutes after a git timeout.

A missed 1 s tool-path deadline keeps only this process off the repo's tool path for 60 s (memory only).

## Configuration

`PRIME_AGENT_WORKSPACE_RECALL=0|off|false|no` disables recall at runtime (no mark, no block).

## Public API

`WorkspaceRecall` (+ `RecallOptions`, `MarkOutcome`, `SkipReason`, `MarkWriter` for embedders and tests), and the
building blocks re-exported from `lib.rs`: `capture_workspace`, `workspace_digest`, `is_fully_verifiable`,
`write_recall_mark`, `read_recall_mark`, `read_recall_skip`, `recall_mark_path`, `recall_skip_path`,
`recall_repo_key`, `witness_workspace`, `render_recall_block`, the claim predicates, and their types.

## Telemetry

No product (`pa-telemetry`) event yet; the TS product reported recall only through its spans.
