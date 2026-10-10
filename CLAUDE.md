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
| Python kernel | `prime-agent-runtime/src/rlm/repl.py` | JSONL protocol **5** with `pa-core::kernel` (`REPL_PROTOCOL_VERSION`) |

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
- Runtime: kernel span tracing, workflow v1/v2, bash activity rows (`bashCommands`, `bash.consumed`), the fd-2 stderr
  tee, the pidfd owner watchdog with a raw-syscall fallback.
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

Branch `merge-rust-port`, pushed to the fork remote (`fork`, `coleleavitt/prime-agent`). Upstream `main` merged
through #3264/#3290, then every open upstream PR (99) was triaged: 56 merged (one merge commit each, resolution
decisions in the commit messages), 43 not merged — 22 TS-only, 14 superseded by a merged Rust port, 5 TS-only in
substance or targeting tooling the port deleted, and #2351 (`rlm.watch`) / #2352 (`messaging_stats`), which need a
Rust host port first. Live handoff notes: `MEMORY.md`.

The fork's TS-only features are ported as removable feature crates (`docs/fork-feature-crates.md`): `pa-trace`,
`pa-recall`, `pa-toolforge`, `pa-dream` (standalone + in-session, `/dream`), `pa-workflow` (V1 host; V2 wire +
`validate`), `pa-ledger`, `pa-ravo` (gate, referee, trust windows, `ravo.run`, `/ravo`), `pa-learning`,
`pa-session-index`, `pa-mermaid`, `pa-anthropic-auth` (the `anthropic` provider's OAuth from the shared
`~/.anthropic-accounts` store via the vendored anthropic-rs SDK). Native upstream-parity Mermaid rendering lives in `pa-tui/src/mermaid/`. On-disk
formats are byte-compatible with the TS fork, proven by node-generated goldens in each crate's `tests/`. Static bug
rules for this repo's hazard classes: `codegraph-rules/` (`codegraph analyze rules`).

Open:
- Workflow V2's durable controller (the TS fork never built it; needs a store dependency decision). The factory
  executor's host-side control loop, child port and restart reconciliation are reusable for it; its JSON record
  store is not the transactional event-sourced store `WORKFLOW-V2.md` specifies.
- Computer use's Rust backends (`crates/pa-computer-use`) are tested over test doubles only (macOS compile-checked
  for aarch64-apple-darwin, never run); none has been live-tested yet. The removed Python Wayland backend was
  live-tested on niri 26.04 with a GTK 4 window; tiled windows lacked coordinate input and screenshots there (niri
  exposes positions only for floating windows), and the Rust Wayland backend refuses coordinate input on a tiled
  window for the same reason (`tiled_windows_refuse_coordinate_input_without_moving_focus`).
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
S=/tmp/pa-gate; mkdir -p $S/home $S/clippy && touch $S/rustfmt.toml   # any scratch dir
export HOME=$S/home PATH=/home/cole/.cargo/bin:$PATH CARGO_HOME=/home/cole/.cargo RUSTUP_HOME=/home/cole/.rustup \
       CLIPPY_CONF_DIR=$S/clippy UV_CACHE_DIR=/home/cole/.cache/uv UV_PYTHON_INSTALL_DIR=/home/cole/.local/share/uv/python
unset PA_TS_BINARY PRIME_AGENT_KERNEL_VENV PRIME_AGENT_KERNEL_PYTHON PA_CORE_KERNEL_PYTHON PA_E2E_KERNEL_PYTHON

cargo +1.98.1 fmt --all -- --check --config-path $S/rustfmt.toml      # rustfmt finds ~/.rustfmt.toml via parents
cargo +1.98.1 clippy --workspace --all-targets --locked -- -D warnings
cargo +1.98.1 clippy --workspace --all-targets --locked --target x86_64-pc-windows-gnu -- -D warnings
cargo +1.98.1 build --locked --workspace --bins && ./target/debug/prime-agent --prime-agent-bootstrap   # kernel venv
cargo +1.98.1 test --workspace --locked --no-fail-fast
cargo +1.98.1 deny --all-features --workspace check advisories licenses
(cd prime-agent-runtime && uv run python -m unittest discover -s test && uv run ruff check src test)
(cd skills/computer-use && PYTHONPATH=src:tests ../../prime-agent-runtime/.venv/bin/python -m unittest discover -s tests)
```

`make check` is upstream's mirror of these gates (it assumes a default environment). The toolchain is pinned to
1.98.1 (CI); the machine default is a nightly. A kernel-backed test run needs the sandbox HOME's venv bootstrapped by
**this** checkout's binary — two checkouts testing concurrently against one venv will clobber each other.

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
