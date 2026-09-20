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

Branch `perf/session-catalog-resume` on `1d3debb66`, 53 commits ahead of `main`, working tree clean apart from local
junk. Live detail and handoff notes are in `MEMORY.md`. Installed build: `0.9.4-fork.1d3debb6`.

- **Committed since `f4afe5b5d`:** the measurable self-improvement gate, Workspace Recall, the Engineer Trajectory
  Index (`PRIME_AGENT_TRAJECTORY_INDEX=0` off), auto-refine scope global by default, and Dream-RSI in full
  (`core/dream/`, `/dream` + the `dream` skill, `DreamRunService`, capability-gated `dream_run_update`, four tasks,
  the dream-vs-fixed experiment, `evals/dream/plot_experiment.py`, `docs/dream-rsi.md`).
- **Dream-RSI, what four real-token runs established** (Sonnet-5, autocorrelation n=64, isolated agent dir):
  - Run 1: 2 of 83 LLM proposals valid. Fixed in `15768af87` (output contract, rejection log, node `origin`).
  - Run 2: 82 of 88 proposals valid; dreaming inert because the objective was mathematically inert: with
    `beta1 == beta2` and `rounds == k1` the cost and parallelism terms cancel exactly. `706fc14f6` replaced V with
    `(1-b3) quality + b3 anytime - b1 (N+oos)/(W k1) + b2 (1 - rounds/k1)`, charged out-of-support cells, added
    per-candidate verdicts, a dreams log, a per-step lever scan, in-session `--seeds`, child `maxOutputTokens` and
    `thinkingLevel`, the plotter's noise floor and dreaming audit.
  - Run 3: collapsed at the first dreaming step. On a one-tree pool whose best was the first probe, `{fixed-rounds,
    beta 1}` won on stop-early credit alone and did one probe per rollout online. `1d3debb66` charges stop-early
    credit at the latest probe/round the same policy was still improving on the OTHER measured trees (none on a
    single tree), guards quality per tree, and adds an online probation that reverts an adopted policy whose first
    redeploy falls below the incumbent's lowest replay best. The run-3 tree is a fixture.
  - The adversarial pass on that fix found the deeper limit and it is documented, not hidden: 22 of 22 replay
    winners adopted on pools of 2 to 4 incumbent-grown trees scored lower online than the incumbent on 40 fresh
    seeds. Replay on a small frozen pool is not predictive; a positive lever gap means a replay-better policy exists,
    not that dreaming helps. Probation is the operative safeguard.
  - Run 4 (3 seeds, 5 rounds, k1 13, build `1d3debb6`, ~$3): verdict **within noise floor**. Paired deltas +0.0104,
    -0.0079, +0.0090 against a control spread of 0.014; the dream arm used 19% fewer probes for comparable quality;
    5 adoptions, 1 reverted by probation, one seed drifted to 2-probe rollouts through adoptions that passed the
    floor narrowly. Mechanism works end to end; improvement unproven at this scale. Details in `MEMORY.md`.
- **Open:**
  - `release:pack` does not build. Run `npm run build` in `packages/coding-agent` first or the tarball ships the old
    `dist/` under a new version stamp.
  - Cost figures in `scratchpad/realrun2/plots/verdict.html` used Opus rates; Sonnet-5 is $2/$10 per M (cache read
    $0.20, write $2.50), so run 2 cost $0.34 to $0.91 and run 3 (aborted) $0.54 to $1.46.
  - Heavy suites last ran 2026-09-18: `test:kernel` 15/15, `test:process` 12 passed, `test:ci` 7581 passed with 4
    failures (two load-flaky in `daemon-supervisor-monitor.test.ts`, two pre-existing on the base commit).
    `npm run check` and 397 dream/refinement/ETI vitest tests pass on `1d3debb66`.
  - The harness benchmark (`evals/`) cannot show learning yet: cold 4/6, warm 4/6; whether auto-refine now writes a
    global lesson is unmeasured.
- **Known behaviour from the trace corpus** (157 374 real spans on this machine), useful as ground truth:
  `extension.hooks` is the hottest span at 46 721 occurrences; a single `context` hook has been observed taking 29 s;
  `kernel.host_request` for `agent_message.list_agents` has p50 ~ 15 s; several span families show children outliving
  their parents. Treat these as measured facts, not guesses.
- Untracked junk: five core dumps in the repo root (`core.5176` 6.2 GB, `core.889387` 5.8 GB, three ~10 MB), plus
  `.cortexkit/`, `.jython_cache/`, `.pi/`. Do not commit any of it; do not delete it without asking.

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
