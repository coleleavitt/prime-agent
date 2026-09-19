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

Branch `perf/session-catalog-resume` on `f4afe5b5d`. Live detail and handoff notes are in `MEMORY.md`.

- **Uncommitted: the self-improvement gate and Workspace Recall.** 57 modified tracked files (coding-agent `src`,
  `test`, `docs` and `.changes`, `prime-agent-runtime`, `FLOWCHART.md`, `docs/`), plus new `src/core/recall/`,
  `extensions/builtin/workspace-recall.ts`, `ravo/python-environment.ts`, three test files and six fragments.
  `evals/run.mjs`, `evals/results/` and `evals/tasks/study-git-correction*` are separate, unrelated work.
  - Refines: `refine.plan` and `refine.apply` detached root spans, and one
    `refinement.committed|rejected|applied_unmeasured` record per refine at apply time. A failure refine that claims
    nothing is `reject_unclaimed`; any other claimless commit applies without touching RAVO state. A cancelled queued
    or requested refine reports `refine_failed` and releases its triggers.
  - Ledger: global by default (`PRIME_AGENT_GLOBAL_LEDGER=0` keeps it per-session). Provisional windows carry a clock
    (`ordinal` or `local-ordinal`), a regressed champion is repaired in its own scope, and a fingerprint is
    non-actionable (counted, never a trigger) when a strict majority of its occurrences were outages, denials,
    network failures or timeouts.
  - Referee: replays run only for skill create/update edits whose imports a verified missing-module or
    missing-distribution probe names, in one sanitized environment shared with the skill dry-run. `not_applicable`
    exists, `no_evidence` fails closed, and verification runs off the turn path (`ravo.replay_verify`).
  - Workspace Recall: a per-repo digest mark on `agent_end`, a `<workspace_recall>` block on a session's first
    `ipython` result, and build claims from the kernel `done` frame's new `bashCommands`.
    `PRIME_AGENT_WORKSPACE_RECALL=0` turns it off.
- **Open on that set:**
  - `npm run check` passes, and 131 related vitest files and the Python runtime suite pass (2026-09-16). Heavy suites
    (`test:kernel`, `test:process`, `test:ci`) and `prime-agent trace` runtime validation were not run.
  - Build claims need the runtime reinstalled: a kernel on the committed `prime-agent-runtime` sends no `bashCommands`,
    so nothing is claimed until the kernel venv re-syncs and live kernels restart.
  - `ravo.run` commits are measured on that run's own evaluators, skip the claimless-commit rules by design, and log no
    `refinement.*` record, so the learning index does not see them.
  - Trust debits never fire: `settleHarnessTrust` has one caller, and it passes no referee verdicts.
- **Known behaviour from the trace corpus** (157 374 real spans on this machine), useful as ground truth:
  `extension.hooks` is the hottest span at 46 721 occurrences; a single `context` hook has been observed taking 29 s;
  `kernel.host_request` for `agent_message.list_agents` has p50 ≈ 15 s; several span families show children outliving
  their parents. Treat these as measured facts, not guesses.
- ~12 GB of untracked junk sits in the repo root (three core dumps, two session HTML exports), and ~34 GB more in
  `packages/coding-agent/core.*` (node test-process dumps from 2026-09-16). Do not commit it; do not
  delete it without asking.

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
