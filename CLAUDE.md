# Prime Agent (fork) — Claude Instructions

## Mission

Prime Agent is a self-improving RLM harness: a coding agent whose model-facing surface is a **persistent Python
kernel** rather than a fixed tool menu, and whose sessions **outlive the client** behind a supervisor daemon. Upstream
(`PrimeIntellect-ai/prime-agent`, branch `main`) ported the product from TypeScript to Rust (`crates/pa-*`, PR #2524);
this fork tracks that port and carries its own kernel-runtime and skill work on top.

Preserve these properties:

- **The kernel is the tool surface.** The model writes Python; `bash()`, edits, skills, MCP and child agents are
  library calls inside `prime-agent-runtime` (`rlm`), with REPL state surviving the session. Do not flatten this into
  plain tool calls. The model-visible surface is pinned in `AGENTS.md` → "Surface contract".
- **The client does not own execution.** The supervisor daemon routes to one worker process per session; closing the
  TUI never stops an agent. Anything that couples a session's life to a client connection is a bug.
- **Parity with the TS product is the definition of done** for user-visible surfaces (see `AGENTS.md` → Merge gates).

## Authority

`AGENTS.md` is upstream's development-rules document and is **authoritative**: crate ownership and dependency
direction, style, tests, lint discipline, performance (no network on the paint path), merge gates, telemetry, branding,
surface contract. Do not restate or contradict it here. Each crate's `README.md` states its scope, non-goals and public
API. Precedence when they disagree: explicit user instruction → `AGENTS.md` → this file → `.claude/rules/`.

## Architecture First

**Crates** (dependency direction and ownership: the table in `AGENTS.md` → Crates): `pa-types` (shared vocabulary) ·
`pa-telemetry` · `pa-agent` (turn loop, queues) · `pa-ai` (providers) · `pa-models` (catalog) · `pa-sandbox` ·
`pa-core` (session engine, tools, skills, kernel manager, settings) · `pa-daemon` (supervisor, workers, wire) ·
`pa-tui` (wire client only, never links the engine) · `pa-cli` (`prime-agent`, composition root, no logic).

**Process boundaries** — know which one you are in before asserting where code runs:

| process | code | notes |
|---|---|---|
| client / TUI | `pa-cli` + `pa-tui` | speaks the daemon wire (`pa-types::daemon`, protocol 7) |
| supervisor | `pa-daemon::supervisor` | owns the socket lease, routing, roster, worker lifecycle; optional TCP listener (#3203) |
| session worker | `pa-daemon::worker` + `pa-core::session_engine` | one per active session; journals to `~/.prime/agent/sessions` |
| Python kernel | `prime-agent-runtime/src/rlm/repl.py` | JSONL protocol **6** with `pa-core::kernel` (`REPL_PROTOCOL_VERSION`) |

**Other trees:** `prime-agent-runtime/` (the `rlm` package the kernel imports), `skills/` (bundled skills, e.g.
`computer-use`, `factory`, `system-router`; both are embedded in the binary by `pa-core/build.rs` and extracted to
`~/.prime/agent/runtime/<hash>/` when no packaged sidecar exists — the live checkout is never read at run time), `install-rust.sh` (installer; its platform map is pinned by
`crates/pa-cli/tests/installer_platform_map.rs`), `docs/` (design docs for the Rust port and the fork's feature
crates; dated TS-era verification records, `docs/evidence/` and `docs/reviews/` are kept as history).

**Fork features live in removable crates** (`docs/fork-feature-crates.md`): one `pa-<feature>` crate each, wired
only in `pa-cli` behind a Cargo feature; `pa-cli --no-default-features` is upstream's native product.

**Extension points, outermost first:** a skill (`skills/<name>/`, Python, no Rust change) → an `rlm` runtime function
backed by a host request → a session-engine hook → core surgery.

**Protocol surfaces are two-sided contracts:** the kernel JSONL frames (`pa-core/src/kernel/protocol.rs` ⇄
`repl.py`, documented in `prime-agent-runtime/src/rlm/repl.md`), the daemon wire (`pa-types::daemon`), and host
request types. The runtime-ready probe (`kernel/bootstrap/venv/probe.rs`, `RUNTIME_READY_CHECK`) asserts the runtime's
callable surface; a unit test pins its protocol number to `REPL_PROTOCOL_VERSION`. Change both sides together.

## What this fork adds on top of upstream

- Kernel protocol 4: the runtime's `host_cancel` and `trace` events (the host parses them; `trace` is logged at debug).
  Protocol 5: the runtime needs a host that serves `bash.*` and `computer_use.*`; the handshake refuses a skewed pair.
  Protocol 6: the out-of-band `detach` request (send-now moves the running cell to the background; `cell.completed`
  reports its end).
- Runtime: kernel span tracing, workflow v1/v2, bash activity rows (`bashCommands`, `bash.consumed`), the fd-2 stderr
  tee, the pidfd owner watchdog with a raw-syscall fallback.
- Queued steers join the running turn at its next boundary (Claude Code's mid-turn delivery) instead of ending the
  run: `pa-core::session_engine::steering_fold` admits them like a busy-session steer (skill and template expansion,
  the features' prompt review), `pa-daemon::worker::steer_fold` takes the lane's plain user steers and settles their
  waiters with the run; session commands, image prompts, injected rows and a refused steer keep the stop-and-restart
  delivery.
- Background work follows Claude Code's model: host notices on the steering lane (`[bash-done]` with the output's
  tail, watch events, `rlm.monitor` line batches, a moved cell's `[cell-done]`, a feature's `feature_notice`) join
  a running turn at its next boundary as their own rows (`steer_fold::FOLDABLE_NOTICE_TYPES`; the engine bridge
  persists and broadcasts them) or wake an idle session. `rlm.monitor` follows a command's output lines (pa-bash
  `bash.output` `sinceBytes`). Send-now (Ctrl+Enter) moves the running cell to the background (`detach`, protocol 6,
  daemon `detach_running_cell`) and interrupts only what cannot move. Factory runs notify on the follow-up lane
  (`task_notification` rows); the bundled `tasks` skill is the model's task list.
- `pa-agent`: queued batches record their origin; an aborted run parks the **user's** steer/follow-up rows (next
  prompt or `continue` folds them) while host rows (terminal notices, injected rows) keep driving the session.
- The factory (`skills/factory`, `rlm.factory`) runs host-side: `pa-core::factory` owns the spec validator, the
  executor (one per session, a durable `factory-runs/<run id>.json` record per run, recovery that pauses in-flight
  runs as interrupted after a host restart) and the `/factory` lane; the kernel's `rlm.factory` is a thin client.
- Spawn hardening: ETXTBSY-tolerant spawns (`platform::process::{status,spawn}_retrying_text_busy`) for the runtime
  probe, `uv`, and staged-update version probes; runtime-probe memo invalidation scoped to the failed interpreter.
- Resume follows Claude Code's picker: `--resume <selector>` / `/resume <selector>` resolve in `pa-cli`
  (`resume_selector`): a file, a saved id (exact, prefix or the 12-character display suffix), then a saved title
  (exact, case-insensitive, this directory first); several matches open the agents view searching for the selector,
  no match attaches a running session and that miss opens the same search. `-c` with nothing to continue says so in
  the agents view. The catalog records each session's git branch (`gitBranch`: the newest `git_state`, else the
  header; `SESSION_ROW_VERSION` 2, sidecar 3); the agents view shows a Branch column, filters to this repository's
  branch (`Ctrl+B`), previews a session's conversation from its file tail (`Ctrl+V`), and searches the branch, the
  whole first message and the transcript corpus below the name/id/cwd tiers (`agents_view_search`). Directory
  comparisons go through `pa_types::platform::same_directory`.
- Computer use runs host-side in `crates/pa-computer-use` behind the `computer_use.*` host requests: macOS (AX,
  `CGEvent`, objc2), X11 (#3246; `xdotool`/`xwininfo`/`maim`|`scrot`) and a **Wayland/niri** backend (niri IPC,
  AT-SPI over `zbus`/`atspi-proxies`, wlr virtual pointer/keyboard over `wayland-client`, `grim`), with the logind
  `LockedHint` lock probe for both Linux backends. `skills/computer-use` is a thin Python client (no pyobjc or
  PyGObject in the kernel).

Claude Code parity port (branch `cc-parity`; no kernel or daemon protocol bump; new daemon capabilities `rewind` and
`external_sessions`; adoption telemetry schema 6: `permissions_session`, `observer_session`, `advisor_ask`,
`lsp_session`, plus custom-command, vim and status-line counters; every new `rlm` function is in the ready probe):

- Context and instructions: the system prompt is byte-stable (static layers, packages, session role, appended
  prompt). Instruction files, skills inventory, MCP servers, environment (cwd, branch, platform, shell, OS, model,
  sandbox, scratchpad) and date ride the first turn as a hidden `session_context` row (`prompts::session_context`),
  later turns get `session_context_update` deltas (`session_engine::context_rows`). Per-turn `<system-reminder>` rows
  (`session_engine::reminders`, switchable via `reminders.*`): changed files (the kernel's additive `fileAccess` on
  `done`), the `tasks` nag, silent turns, elapsed time, `<total_tokens>`, re-reads. One instruction loader
  (`pa-core::resources::instructions`, `docs/instruction-files.md`): AGENTS.md/CLAUDE.md families root-to-cwd plus
  `.claude/CLAUDE.md`, `*.local.md`, `.prime/agent/rules`, `.claude/rules` (`~/.claude` only with
  `claudeCodeUserInstructions`), `@path` imports (depth 5), `paths:`-scoped rules; subdirectory files arrive as a
  hidden `nested_instructions` row once a cell touches them (`accessedPaths`, `session_engine::instruction_boundary`).
  Auto memory (`resources::auto_memory`, on by default for top-level sessions; `autoMemoryEnabled: false`,
  `PRIME_AGENT_DISABLE_AUTO_MEMORY`, `--no-context-files`): `<agent dir>/projects/<checkout>/memory/MEMORY.md`
  (200 lines / 25 KB), its directory name exactly Claude Code's; `/memory` lists both.
- Checkpoints and `/rewind` (on by default; `fileCheckpointingEnabled`, `PRIME_AGENT_DISABLE_FILE_CHECKPOINTING`): the
  `edit` skill reports each write (`file_history.before_write`/`after_write`, host-internal) and
  `pa-core::file_history` keeps prior bytes content-addressed under `<session artifacts>/file-history/` (bash and
  plain Python writes are not tracked; a file changed outside the agent is skipped unless forced). Daemon
  `get_rewind_checkpoints`/`rewind` (`pa-daemon::rewind`) restore code, then the conversation via `navigate_tree`.
  TUI: `/rewind` (`/checkpoint`, `/undo`) and Esc Esc on an empty editor (`rewind_selector`); old daemons get `/tree`.
- Agents and messaging: a child that ends without replying, fails or is deleted reaches its parent as a
  `<task-notification>` on the steering lane; agent messages fold mid-turn. `rlm.send_message` / `rlm.list_agents`.
  Typed agent definitions (`.prime/agents`, `.claude/agents`; `pa-core::agents`) via `rlm.spawn(agent=...)`:
  instructions appended, model/effort defaults, `Bash`/`Agent`/`SendMessage` scope enforced by a host-request gate,
  persisted as an `agent_definition` entry; a definition's `memory: user|project|local` gives it a persistent
  `agent-memory/<agent>/` (`pa_core::agents::memory`). `isolation="worktree"` children use `pa_core::worktree`
  (removed with the child only when unchanged).
- Coordinator and brief modes (Claude Code's unreleased, both off;
  `pa-core::session_engine::{coordinator,brief_mode,modes}`): `experimental.coordinatorMode` /
  `PRIME_AGENT_COORDINATOR_MODE` appends CC's coordinator prompt and gates `bash.run`, `file.edit`, `web.*`, MCP,
  `computer_use.*` and worktree moves (unless `coordinatorExtraTools`); `experimental.briefMode` / `PRIME_AGENT_BRIEF`
  / `/brief` adds `rlm.send_user_message` (a displayed `user_message` row, "● Message" in the TUI) and collapses
  plain assistant text.
- Other flag-gated Claude Code behaviors under `experimental` (`pa-core::settings::experimental`, `PRIME_AGENT_*`
  overrides; side calls via `session_engine::side_model`). On: output-limit recovery (up to 3 hidden resume nudges;
  `PRIME_AGENT_OUTPUT_LIMIT_RECOVERY=0`). Off: `usageLimitWrapUp`, `goalEvaluator`, `modelProposedGoals`
  (`goal.propose`), `idleCompaction`, `autocompactOffer` (CC 2.1.310: a threshold compaction asks on the
  confirmation surface first — `session_engine::autocompact_offer`, `SessionEngine::offer_autocompaction`, daemon
  `run_auto_compaction`; a declined offer is suppressed until the context drops, a surfaceless session compacts
  silently, overflow never asks), and the feature crates `pa-observer` (`observer`: per-turn digests to a
  background observer, reports as notices) and `pa-advisor` (`advisorModel`: the `advisor` skill's `advisor.ask`).
- Web (`crates/pa-web`, feature `web`): `rlm.web_search` / `rlm.web_fetch` (`web.search` / `web.fetch`). Search via
  Anthropic's server-side `web_search_20250305` on the session's Anthropic login (through the generic
  `pa_ai::types::StreamOptions::on_event` seam), else Brave or Exa with a key (`web.*`, `BRAVE_API_KEY`,
  `EXA_API_KEY`). Fetch: GET to Markdown (`htmd`), https upgrade, private hosts refused, same-host redirects, 15-minute
  cache, untrusted-content fence, 100k-character windows; an optional `prompt` uses `web.fetchModel`.
- Permissions (`crates/pa-permissions`, feature `permissions`; inert without rules): CC's `permissions.allow/ask/deny`
  syntax and validator over `bash()` (compound commands part by part, paths against Edit/Read rules), `file.edit`,
  `web.*`, MCP and tool calls; deny > ask > mode > allow > read-only > fallback (unmatched runs unless
  `"fallback": "ask"`); modes default/acceptEdits/plan/bypassPermissions/dontAsk, shift+tab cycles via the
  `permissions` feature status; `/permissions`; always-allow answers (`features::confirm_offering`, `remember`)
  persist to `.prime/agent/settings.local.json`; untrusted projects contribute only deny/ask. Python run directly in
  the kernel is not gated.
- Worktrees and scratch: `pa_core::worktree` is the one helper (sessions, children, workflows) for
  `<repo>/.prime/worktrees/<name>` on `worktree-<name>` (base `origin/HEAD`, or HEAD with `worktree.baseRef: "head"`;
  `.worktreeinclude` copied; never destructive by default; pa-hooks WorktreeCreate/WorktreeRemove can replace git).
  `prime-agent -w, --worktree [name]`; `rlm.enter_worktree` / `rlm.exit_worktree` (pa-daemon `worktree_host`) move a
  running session like `/cwd`. Each session has a private `scratchpad/` and `tasks/` (`PRIME_AGENT_SCRATCHPAD`,
  `PRIME_AGENT_TASKS_DIR`); every `bash()` output is saved to `tasks/<job id>.output` (pruned to 200 files / 512 MiB).
- Workflows and review: `rlm.workflow` (`rlm/workflow_script.py`) runs CC's async Workflow scripts
  (`agent`/`pipeline`/`parallel`/`phase`/`log`/`args`/`budget`, schema results with a repair retry) over `rlm.spawn`
  children, journaled under `<session>/workflows/<wf id>/` for `resume_from`; `start()` backgrounds a run. Saved
  workflows resolve `<name>.py` from `.prime/workflows/`, `.claude/workflows/`, `<agent dir>/workflows/`,
  `~/.claude/workflows/` (`/workflows`). The bundled `code-review` skill (`/code-review`, `/review`) and
  `workflow-authoring`. `session_engine::ultracode`: the `ultracode` keyword reminder
  (`workflowKeywordTriggerEnabled`) and the standing `/ultracode` mode.
- Small surfaces: `.claude/commands` custom commands as prompt templates (CC argument rules, `` !`cmd` `` blocks
  through the `bash()` guards, `disableSkillShellExecution`); output styles (`outputStyle`, `/output-style`); the
  TUI's `statusLine` off the paint path; vim mode (`editorMode`, `/vim`, `pa-tui/src/editor/vim.rs`); additional
  directories (`--add-dir`, `/add-dir`, `permissions.additionalDirectories`) as one persisted session list.
- Scheduling: `rlm.cron.create/list/delete`, `rlm.schedule_wakeup`, `/loop [interval] <prompt>|stop`.
  `pa_core::cron::tasks` is a pure task book (local-time five-field cron, CC's jitter, 7-day expiry, wakeups clamped
  to [60, 3600]); durable tasks in the session artifacts' `scheduled_tasks.json`. pa-daemon's `session_tasks` fires
  them only while idle, as `scheduled_task_fire` rows on the follow-up lane; pending tasks keep the worker resident.
  `PRIME_AGENT_DISABLE_CRON`, `PRIME_AGENT_LOOP_PERSISTENT`, `PRIME_AGENT_LOOP_KEEPALIVE=0`.
- Claude Code settings, opt-in (`claudeCodeSettings: true` in user settings): `pa_core::settings::claude_code` is the
  one reader of CC's managed, `.claude/settings{,.local}.json` and `~/.claude/settings.json` (CC precedence; project
  files only when trusted, `GatedItem::ClaudeSettings`). pa-permissions, pa-hooks (source-labelled hooks,
  `disableAllHooks`, `allowManagedHooksOnly`) and the `.mcp.json` approvals read through it; `SettingsManager` layers
  `statusLine`, `outputStyle`, `model`, `cleanupPeriodDays`, `additionalDirectories` and `env` (to kernel and hooks;
  harness variables win) under the agent's own settings.
- Claude Code skills (`pa-core::skills::claude_skills`): `.claude/skills/<name>/SKILL.md` cwd-to-repo-root
  (trust-gated) and additional directories; `~/.claude/skills` only with `claudeCodeUserSkills`. Instruction-only, a
  prime-agent skill of the same name wins; `/<name>` or `/skill:<name>`; `rlm.load_skill(name, args)` (`skill.load`)
  returns CC's invocation text; touched subdirectories' skills arrive as a hidden `dynamic_skills` row.
- `.mcp.json` (`pa-core::mcp::declared`, `docs/mcp-json.md`): every `.mcp.json` root-to-cwd (nearest wins), plus
  `~/.claude.json` user/local servers when opted in; CC validation and `${VAR:-default}` expansion. The file is a
  workspace-trust gated project file (an untrusted repo's `.mcp.json` is ignored; editing it asks again), and each
  project server still needs approval (`enabledMcpjsonServers` and friends, `<agentDir>/mcp-project-choices.json`,
  the confirm panel). `/mcp` shows source and approval; `prime-agent mcp add/remove --scope project`, `mcp list`,
  `mcp reset-project-choices`.
- LSP (`crates/pa-lsp`, feature `lsp`; inert without servers): `lspServers` or a trusted `.lsp.json`
  (`workspace_trust::register_gated_project_file`); servers start on first use and stop with the session.
  `rlm.lsp.{definition, implementation, references, hover, document_symbols, workspace_symbols, call_hierarchy,
  diagnostics, servers}`; after a cell writes files (python_repl `fileWrites` host fact) new diagnostics arrive as a
  hidden `<new-diagnostics>` reminder (`lspDiagnosticsWaitMs`).
- Claude Code sessions (`crates/pa-cc-import`, feature `cc-import`): the agents view lists `~/.claude/projects`
  transcripts in a "Claude Code" section (daemon `list_external_sessions` / `import_external_session`, via the
  `pa_core::session::external_sessions` seam); opening one, or `--resume <cc uuid | title>`, converts it into a new
  session file (`importedFrom`; tool calls as text plus `claude_code_tool_result` rows, thinking dropped). The source
  transcript is never written; an unchanged session reuses its import.

## Current State

Branch `merge-rust-port`, pushed to the fork remote (`fork`, `coleleavitt/prime-agent`). Upstream `main` is merged
through `5f218f780` (#3591, past v0.10.1-beta.20; merges `ea2530012`, `8b83861ca` and `bd0564dec`, their resolution
decisions in the commit messages): the
Decision API revert (#3522) followed, `serde-saphyr` kept over upstream's `yaml_serde`, the `ipython` -> `python_repl`
rename (#3400) followed. Upstream merges go through a merge driver that formats base/ours/theirs in the fork's style
first, and `Cargo.lock` is rebuilt as the union of both locks pruned by cargo. Live handoff notes: `MEMORY.md`.

Branch `cc-parity` (local only, not pushed) holds the Claude Code parity port on top of `9a9ab23cb`: seventeen feature
branches merged in two rounds, with integration fixes in their own commits (one worktree helper, `.mcp.json` through
the `claude_code` reader and behind workspace trust, the hold-refusal classifier accepting `target/<triple>/<profile>`).

The fork's TS-only features are ported as removable feature crates (`docs/fork-feature-crates.md`): `pa-trace`,
`pa-recall`, `pa-toolforge`, `pa-dream` (standalone + in-session, `/dream`), `pa-workflow` (V1 host; V2 wire +
`validate`), `pa-ledger`, `pa-ravo` (gate, referee, trust windows, `ravo.run`, `/ravo`), `pa-learning`,
`pa-session-index`, `pa-mermaid`, `pa-anthropic-auth` (the `anthropic` provider's OAuth from the shared
`~/.anthropic-accounts` store via the vendored anthropic-rs SDK). The Claude Code parity port added `pa-web`,
`pa-permissions`, `pa-observer`, `pa-advisor`, `pa-lsp` and `pa-cc-import` (all in pa-cli's default features;
observer and advisor stay inert until their `experimental` settings name a model). Native upstream-parity Mermaid rendering lives in `pa-tui/src/mermaid/`. On-disk
formats are byte-compatible with the TS fork, proven by node-generated goldens in each crate's `tests/`. Static bug
rules for this repo's hazard classes: `codegraph-rules/` (`codegraph analyze rules`).

Fork-only (no TS counterpart; upstream removed its TypeScript extension API in #3189): `pa-hooks` (Claude
Code-compatible command hooks from the `hooks` settings key; the kernel's `bash()` is the `Bash` tool, `PostToolUse`
included) and `pa-plugins` (WebAssembly components on wasmtime, contract `wit/plugin.wit` 0.2.0: policy on tool calls
and kernel host requests, after-run notes, prompt context, status panels under the editor, model tools called through
the `wasm-plugins` skill, `/wasm` commands; Rust example `examples/plugins/command-guard`). Both plug into generic
`pa-core` seams (`features/`: `review_tool_call`, `review_host_request`, `after_host_request`, `prompt_context`,
`before_agent_stop`, `after_tool_batch`, `before`/`after_compaction`, `on_session_end`, `deliver_feature_notice`,
`register_gated_project_dir`). An `Ask` verdict goes to the user through `features::confirm`: the `confirmation`
feature status, the TUI's yes/no panel, and the `answer_feature_confirmation` daemon command (capability
`feature_confirmation`); print mode refuses. Contract in `docs/hooks-and-plugins.md`.

Open:
- Workflow V2's durable controller: new work, not a port (the TS fork stopped after slices 2-4, which exist here
  dormant: the SQLite store, reducer, capture/settlement, dispatch guard). Remaining are `WORKFLOW-V2.md` slices
  5-9 (controller/executor adapter and create/start, settlement policy/acceptance/budgets/retry, cancellation and
  the crash-window suite, the Python/status/events/view projections, release attestation) plus slice 3's native
  halves in `pa-daemon` (composite request-id admission, the retained child runtime). The factory executor's
  host-side control loop, child port and restart reconciliation are reusable for it.
- Computer use's Rust backends (`crates/pa-computer-use`) are tested over test doubles only (macOS compile-checked
  for aarch64-apple-darwin, never run); none has been live-tested yet. The removed Python Wayland backend was
  live-tested on niri 26.04 with a GTK 4 window; tiled windows lacked coordinate input and screenshots there (niri
  exposes positions only for floating windows), and the Rust Wayland backend refuses coordinate input on a tiled
  window for the same reason (`tiled_windows_refuse_coordinate_input_without_moving_focus`) unless niri is the
  computer-use fork (`WindowGeometry`/`CaptureWindow`/`WindowAt`, contract in the niri fork's `CONTRACT.md`).
- `make check`'s MSVC lane needs `cargo-xwin` (not installed here); the gnu Windows lane passes.
- `cargo deny` advisories fail on RUSTSEC-2026-0370/0371/0372 (`zbus_names`, `zvariant`, `zvariant_derive`
  unmaintained, via `pa-computer-use`); licenses pass. Ignore in `deny.toml` or replace: the user's call.
- The `attach-image` and `present-artifact` skill tests need Pillow (`PIL`), missing from the runtime venv.

## Working Rules

- **Git (shared worktree):** stage specific paths only (`git add -u -- <paths>` for tracked files — plain `git add`
  exits 1 on `crates/pa-core/src/kernel/bootstrap/venv/` because a global ignore matches `venv/`). Never `git add -A`,
  `git add .`, `git stash`, `git reset --hard`, `git checkout .`, `git clean -fd`, `--no-verify`, force pushes, or
  identity overrides (`AGENTS.md` → Repository).
- **Do not push or open PRs** unless asked; the upstream repo is the org's.
- Tests must be hermetic against this machine (see `.claude/rules/testing.md`): global git config, gitignore, TZ,
  `PI_OFFLINE`, backtrace env and the TS binary on PATH have all leaked into tests before. So has the real agent
  state: a spawned binary gets `pa_types::platform::test_isolation::TestState` (temp agent dir, account store, Claude
  config, test `HOME`), and the agent-dir/kernel-venv resolvers and binary startups refuse the passwd home's
  `~/.prime`, `~/.anthropic-accounts` and `~/.claude` in any test process (`PA_TEST_PROTECTED_HOME` adds a sentinel).
- Ask before removing functionality that looks intentional. No emoji in commits, code, or docs.

## Validation

```sh
S=/tmp/pa-gate; mkdir -p $S/home $S/clippy   # any scratch dir
export HOME=$S/home PATH=/home/cole/.cargo/bin:$PATH CARGO_HOME=/home/cole/.cargo RUSTUP_HOME=/home/cole/.rustup \
       CLIPPY_CONF_DIR=$S/clippy UV_CACHE_DIR=/home/cole/.cache/uv UV_PYTHON_INSTALL_DIR=/home/cole/.local/share/uv/python
unset PA_TS_BINARY PRIME_AGENT_KERNEL_VENV PRIME_AGENT_KERNEL_PYTHON PA_CORE_KERNEL_PYTHON PA_E2E_KERNEL_PYTHON

cargo +nightly fmt --all -- --check --config-path /home/cole/.rustfmt.toml   # HEAD's style (HOME is the sandbox here)
cargo +1.98.1 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.98.1 clippy --workspace --all-targets --locked --target x86_64-pc-windows-gnu -- -D warnings
T=$(cargo +1.98.1 metadata --format-version 1 --no-deps | jq -r .target_directory)   # not always ./target
cargo +1.98.1 build --locked --workspace --bins && $T/debug/prime-agent --prime-agent-bootstrap   # kernel venv
cargo +1.98.1 test --workspace --locked --no-fail-fast
cargo +1.98.1 deny --all-features --workspace check advisories licenses
(cd prime-agent-runtime && uv run python -m unittest discover -s test && uv run ruff check src test)
(cd skills/computer-use && PYTHONPATH=src:tests ../../prime-agent-runtime/.venv/bin/python -m unittest discover -s tests)
```

Formatting: HEAD is formatted with nightly rustfmt and the user's `~/.rustfmt.toml` (vertical import lists). Format a
touched file through stdin, so its `mod` declarations are not followed into untouched files:
`rustfmt +nightly --edition 2021 --config-path /home/cole/.rustfmt.toml < f > f.tmp && mv f.tmp f`.

`make check` is upstream's mirror of these gates (it assumes a default environment). The toolchain is pinned to
1.98.1 (CI); the machine default is a nightly. A kernel-backed test run needs the sandbox HOME's venv bootstrapped by
**this** checkout's binary — two checkouts testing concurrently against one venv will clobber each other.

On this machine every checkout under `forks/` shares one build directory (`forks/.cargo/config.toml`, outside the
repo: `forks/target`; keep the name `target`: dev-build detection keys on it), so worktrees also share `debug/prime-agent` and the last build wins. Superseded test
binaries are never deleted by cargo and reached 634 GB on 2026-10-15: check its size before a full test run and
prune with `cargo sweep --maxsize 150GB`. The runtime finds the checkout's binary in cargo's build directory
(`CARGO_TARGET_DIR`, else the nearest `build.target-dir` in a `.cargo/config.toml`; `rlm._host.cargo_target_dir`).
The example plugin has its own gate, `examples/plugins/command-guard/check.sh` (needs the `wasm32-wasip2` target,
installed for 1.98.1 here).

## Project Claude Layout

`.claude/` and `MEMORY.md` are gitignored: local to this checkout.

```
CLAUDE.md               this file (primary, tracked)
AGENTS.md               upstream development rules (authoritative, tracked)
MEMORY.md               local state and handoff notes
.claude/CLAUDE.md       entrypoint pointing here
.claude/rules/          code-style · testing · security
.claude/agents/         researcher (read-only) · verifier (runs the gates)
.claude/commands/       /audit · /repro
```
