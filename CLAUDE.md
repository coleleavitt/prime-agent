# Prime Agent — Claude Instructions

## Mission

Prime Agent is a self-improving RLM harness: a coding agent whose model-facing surface is a **persistent Python
kernel** rather than a fixed tool menu, whose sessions **outlive the client** behind a daemon, and which carries **one
W3C `traceparent` per user turn** across every process and both languages.

Those three properties are the product. Preserve them:

- **The kernel is the tool surface.** The model writes Python; `bash()`, file edits, skills, MCP and child agents are
  library calls inside it, with REPL state surviving the whole session. Do not "simplify" this into flat tool calls.
- **The client does not own execution.** A supervisor daemon routes to resident session workers. Closing the TUI must
  never stop an agent. Anything that couples a session's life to a client connection is a bug.
- **Observability is a contract, not a nicety.** Every cross-process hop opens a span and has a row in
  `docs/observability.md`. `~/.prime/agent/logs/agent.jsonl` is the system of record for runtime behaviour.

This is a fork of `PrimeIntellect-ai/prime-agent`. Upstream conventions in `AGENTS.md` are authoritative; keep changes
rebasable.

## Architecture First

Read `FLOWCHART.md` before making a structural change — it is the one-page map and it is meant to stay current.

**Layers, outermost first. Dependencies point inward only.**

| package | published as | owns | must not |
|---|---|---|---|
| `packages/tui` | `@earendil-works/pi-tui` | terminal widgets | know anything about agents |
| `packages/ai` | `@earendil-works/pi-ai` | providers, streaming, model catalogue, trace context, OTLP export | gain heavy dependencies |
| `packages/agent` | `@earendil-works/pi-agent-core` | the turn loop only | import from `coding-agent`; know about daemons or terminals |
| `packages/coding-agent` | `@earendil-works/pi-coding-agent` | sessions, daemon, kernel bridge, extensions, CLI | let `core/` import a `mode/` |
| `prime-agent-runtime/` | bundled into coding-agent | model-facing Python (`repl.py`, `bash.py`, `mcp.py`, `trace.py`) | drift from the TS frame contract |

Inside `packages/coding-agent/src`: `core/` is logic, `modes/` is a runnable mode, `cli/` is argv and command
registration. **Modes may import core; core must never import a mode.**

**Process boundaries** — establish which one you are in before asserting where code runs: client/TUI · daemon
supervisor · session worker · Python kernel child · catalog process. Every span carries `mode` and `pid`.

**Extension points, in preference order.** Reach for the outermost one that works:

1. a skill (`packages/coding-agent/skills/<name>/`) — Python, no TypeScript change
2. an extension hook (`core/extensions/`) — `context`, `turn_start`, `tool_execution_*`, `agent_end`, …
3. an `AgentLoopConfig` hook (`getSystemPrompt`, `getRequestContext`, `getApiKey`, `getSteeringMessages`,
   `beforeToolCall`, `onPayload`, `onResponse`) — how `coding-agent` steers `agent` without coupling them
4. core surgery — last resort; `core/agent-session.ts` is ~13 300 lines with hundreds of callers

**Protocol surfaces are versioned and gated.** Daemon commands/events, kernel JSONL frames, and host request types are
all two-sided contracts. See `.claude/rules/security.md` for the daemon versioning procedure. Never make a new daemon
command part of startup without a capability gate.

## Current State

Branch `perf/session-catalog-resume` on `15768af87`, 50 commits ahead of `main`, working tree clean apart from local
junk. Live detail and handoff notes are in `MEMORY.md`. Installed build: `0.9.4-fork.15768af8`.

- **Committed since `f4afe5b5d` (five commits):** the measurable self-improvement gate (refine spans and
  `refinement.*` records, global-by-default ledger, referee replays, retired replay-case pruning, trust debits,
  `ravo.run` logging, stale-evidence re-plan, rejection history), Workspace Recall, the Engineer Trajectory Index
  (`core/distill/trajectory-index.ts`, `learning trajectory`, `PRIME_AGENT_TRAJECTORY_INDEX=0` off), auto-refine
  scope global by default (only an explicit `"local"` stays session-scoped), and Dream-RSI in full: `core/dream/`,
  `/dream` + the `dream` skill, `DreamRunService`, the capability-gated `dream_run_update` daemon event (schema
  revision 32), four tasks (`circle-packing`, `sum-difference`, `python-speedup`, `autocorrelation`), the
  dream-vs-fixed experiment with LLM and guidance arms, `evals/dream/plot_experiment.py`, `docs/dream-rsi.md`.
- **Dream-RSI, measured honestly.** The replay objective is scale-invariant (`q` normalized to the pool range,
  β1 = β2 = 0.05, quality guard) and seeds are deterministic; on circle-packing the dream arm ties the fixed arm at
  equal budget with fewer probes. Replay can only reward spending less, never finding more (out-of-support limit), so
  the LLM proposer is the only lever for quality gains. The first real-token run (Sonnet-5, autocorrelation, $1.70)
  was null because 81 of 83 LLM proposals fell back to the local proposer; `15768af87` fixes the output contract and
  persists every rejection (`<dream dir>/rejections/<run>.jsonl`, `dream.llm_reject_reason`, node `origin`). A
  re-run on that build is the open measurement.
- **Open:**
  - `release:pack` does not build. Run `npm run build` in `packages/coding-agent` first or the tarball ships the old
    `dist/` under a new version stamp (this happened once; the install was repeated).
  - Child agents have no hard output cap or thinking override (`RunAgentOptions` has only `maxTurns`, `tokenBudget`);
    a proposer that writes 30k tokens of prose stops on `length`. Documented in `docs/dream-rsi.md`.
  - `dream.propose`/`dream.llm_propose` records only the last rejection reason of a retried attempt; the earlier one
    lives only in the rejection log.
  - Heavy suites last ran 2026-09-18: `test:kernel` 15/15, `test:process` 12 passed, `test:ci` 7581 passed with 4
    failures — two in `daemon-supervisor-monitor.test.ts` that pass in isolation (111/111, load-flaky) and one each in
    `stdin-guard-cold-cli.test.ts` and `regressions/4603-worker-recovery.test.ts` that fail identically on the base
    commit (pre-existing). `npm run check` and the dream/refinement/ETI vitest files pass on `15768af87`.
  - The harness benchmark (`evals/`) cannot show learning yet: cold 4/6, warm 4/6. `mem-off` 0/3 vs `mem-on` 3/3
    shows retrieval works once a lesson is global; whether auto-refine now writes one is unmeasured.
- **Known behaviour from the trace corpus** (157 374 real spans on this machine), useful as ground truth:
  `extension.hooks` is the hottest span at 46 721 occurrences; a single `context` hook has been observed taking 29 s;
  `kernel.host_request` for `agent_message.list_agents` has p50 ≈ 15 s; several span families show children outliving
  their parents. Treat these as measured facts, not guesses.
- Untracked junk: five core dumps in the repo root (`core.5176` 6.2 GB, `core.889387` 5.8 GB, three ~10 MB), plus
  `.cortexkit/`, `.jython_cache/`, `.pi/`. The `packages/coding-agent/core.*` dumps and session HTML exports are gone.
  Do not commit any of it; do not delete it without asking.

## Working Rules

`AGENTS.md` is the authoritative development-rules document — changelog fragments, the provider-addition checklist,
the release process, GitHub/PR workflow, and the parallel-agent git rules. Read it; do not restate or contradict it
here. The rules most often broken, repeated only for emphasis:

- **No inline imports.** No `await import()`, no `import("pkg").Type` in type position.
- **Never edit** `packages/ai/src/models.generated.ts` or `packages/*/CHANGELOG.md`. Edit the generator, or add a
  `.changes/<slug>.md` fragment.
- **No hardcoded keybindings.** Add defaults to `DEFAULT_EDITOR_KEYBINDINGS` / `DEFAULT_APP_KEYBINDINGS`.
- **Read files in full** before wide-ranging changes or any investigation. Search snippets are not enough here.
- **Multiple agents share this worktree.** `git add <specific paths>` only. Never `git add -A`, `git add .`,
  `git stash`, `git reset --hard`, `git checkout .`, `git clean -fd`, or `--no-verify`.
- **Ask before removing functionality** that looks intentional. Do not preserve backward compatibility unless asked.
- No emoji in commits, issues, PR comments, or code. Technical prose, kind but direct.

Detail lives in `.claude/rules/` — [code-style](.claude/rules/code-style.md) ·
[testing](.claude/rules/testing.md) · [security](.claude/rules/security.md).

## Validation

```sh
# after any code change — full output, never tail it. Fix every error, warning and info.
npm run check                 # biome --error-on-warnings + tsgo --noEmit + installer + browser-smoke

# one test file, from the PACKAGE root (not the repo root)
cd packages/<pkg> && npx tsx ../../node_modules/vitest/dist/cli.js --run test/<file>.test.ts
```

**Never run** `npm run dev`, `npm run build`, or a bare `npm test`. Heavy suites (`test:kernel`, `test:process`,
`test:ci`) only when asked by name.

`npm run check` does not run tests. If you created or modified a test file, running it is mandatory.

**Runtime validation**, when a change touches a span, a cross-process hop, or the daemon:

```sh
prime-agent trace <traceId>   # reconstruct one trace from ~/.prime/agent/logs/agent.jsonl
```

Check that every span ends, no child outlives its parent, and `status` is `error` when the operation failed. Aggregate
the log with `python3`; never `cat` it into context.

## Project Claude Layout

`.claude/` is gitignored in this repo, so everything under it is local to this checkout.

```
CLAUDE.md              this file — primary guide
AGENTS.md              upstream development rules (authoritative, do not duplicate here)
FLOWCHART.md           living architecture map; update it when a box or arrow moves
MEMORY.md              local scratch state and handoff notes
.claude/
  CLAUDE.md            entrypoint pointing back here
  agents/researcher.md read-only investigation; source + span-log evidence
  agents/verifier.md   runs the gates, reports real output
  commands/audit.md    architecture and correctness audit workflow
  commands/repro.md    reproduce-before-fix workflow
  rules/               code-style · testing · security
```

When these disagree, precedence is: explicit user instruction → `AGENTS.md` → `CLAUDE.md` → `.claude/rules/`.
