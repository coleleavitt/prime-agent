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
- Computer use runs host-side in `crates/pa-computer-use` behind the `computer_use.*` host requests: macOS (AX,
  `CGEvent`, objc2), X11 (#3246; `xdotool`/`xwininfo`/`maim`|`scrot`) and a **Wayland/niri** backend (niri IPC,
  AT-SPI over `zbus`/`atspi-proxies`, wlr virtual pointer/keyboard over `wayland-client`, `grim`), with the logind
  `LockedHint` lock probe for both Linux backends. `skills/computer-use` is a thin Python client (no pyobjc or
  PyGObject in the kernel).

## Current State

Branch `merge-rust-port`, pushed to the fork remote (`fork`, `coleleavitt/prime-agent`). Upstream `main` is merged
through `5f218f780` (#3591, past v0.10.1-beta.20; merges `ea2530012`, `8b83861ca` and `bd0564dec`, their resolution
decisions in the commit messages): the
Decision API revert (#3522) followed, `serde-saphyr` kept over upstream's `yaml_serde`, the `ipython` -> `python_repl`
rename (#3400) followed. Upstream merges go through a merge driver that formats base/ours/theirs in the fork's style
first, and `Cargo.lock` is rebuilt as the union of both locks pruned by cargo. Live handoff notes: `MEMORY.md`.

The fork's TS-only features are ported as removable feature crates (`docs/fork-feature-crates.md`): `pa-trace`,
`pa-recall`, `pa-toolforge`, `pa-dream` (standalone + in-session, `/dream`), `pa-workflow` (V1 host; V2 wire +
`validate`), `pa-ledger`, `pa-ravo` (gate, referee, trust windows, `ravo.run`, `/ravo`), `pa-learning`,
`pa-session-index`, `pa-mermaid`, `pa-anthropic-auth` (the `anthropic` provider's OAuth from the shared
`~/.anthropic-accounts` store via the vendored anthropic-rs SDK). Native upstream-parity Mermaid rendering lives in `pa-tui/src/mermaid/`. On-disk
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
