# pa-cli

The `prime-agent` binary.

## Scope
Argument parsing, mode selection (interactive/headless/json/daemon subcommands), binary wiring of crates into processes, exit codes and user-facing errors. The daemon-forensics CLI (`incident.rs`, TS `src/cli/incident.ts`): the `prime-agent incident` timeline command — log-source discovery (agent.jsonl plus its `.old` rotation, falling back to the newest per-daemon log), `--since`/`--until` window and `--session` filter parsing, and the report rendering; the shared classifier lives in pa-types (`pa_types::incident`) and the agents-view notice in pa-tui. Client-side MCP auth flows (`mcp_login.rs`): the `/mcp login`/`/mcp logout` hook the TUI calls — the TS interactive client's placement — resolving a server (settings `mcpServers` + the builtin catalog), running the pa-core OAuth login on the suspended terminal, and persisting through the shared auth store. The same placement serves the TUI's client-command hooks: provider auth (`provider_login.rs` — the `/login`/`/logout` catalog, the API-key store, the MCP device flow, the Prime Inference terminal login (`prime_inference_login.rs` — the API-key flow: prime-cli credential reuse, the paste prompt with the whoami access check, the numbered team selector); the subscription OAuth and the Prime browser challenge stay unported and report it), trace-sharing state (`client_traces.rs` — the settings flag writes and the credential resolution `/traces` shows; the upload subsystem stays unported), and the `/update` runner (`client_update.rs` — the out-of-band installer funnel with the output captured, the same body `prime-agent update` runs; `installer_update.rs` is that CLI command's own driver, `--check` included), plus the staged update flow (`update_flow/`, `self_update.rs` — the `package update` self target's body). Tailscale detection (`tailscale/`, TS `src/cli/tailscale.ts`): the `prime-agent tailscale` command group (`status` with `--json`, `serve --port <n> [--funnel]` with post-serve verification) and the `doctor` detection fact, over a bounded subprocess probe of the `tailscale` binary resolved to an absolute trusted path (`probe_tailscale` is the detection seam the tailnet agent mesh builds on; the mesh layers stay unported).

## Non-goals
No business logic; it composes pa-daemon, pa-core, pa-tui, pa-ai via their public APIs only.

## Public API
The `prime-agent` executable surface (flags, subcommands, exit codes). Internal lib types `pub(crate)`, plus the daemon wiring (`interactive_mode::{ensure_daemon_running, ensure_daemon_running_with, resolve_socket_path}`) exported for the integration verifier.

## Depends on
pa-types, pa-ai, pa-agent, pa-core, pa-daemon, pa-tui (one-way, composition root).

## Workspace trust
`workspace_trust_gate.rs` runs before any session starts for the cwd. A workspace
whose gated project configuration (see `pa_core::workspace_trust`) has no
decision for its current content is asked about once on the interactive
terminal (`y` trusts; any other answer is remembered as a denial; end of input
decides nothing); print/json/rpc/acp modes never ask - they print one notice to
stderr naming what was skipped and run without it. `--trust-workspace` records
trust for the cwd before the run. `prime-agent trust [path] [--list]` and
`prime-agent untrust [path]` manage decisions ahead of time. The supervisor and
its workers never ask: they read the recorded decision. Each recorded decision
emits `workspace_trust_decision` (source, decision, whether the content
changed) from a detached thread, so the launch never waits on delivery.

## Print runtime
The headless print/json modes (`print_runtime.rs`) are wiring over
pa-core's daemonless print runtime (`pa_core::session_engine::headless`,
`pa_core::session_engine::rlm_in_process` inline host): this crate selects and
leases the session, assembles every engine (`build_engine`, the root's and, through
the `InlineChildEngineBuilder` seam, each inline child's: provider target, settings,
MCP manager, telemetry, over pa-core's headless model selection and image routing; `print_runtime/faux.rs` is the
`PRIME_AGENT_FAUX_SCRIPT` verification build), converts the autonomous CLI flags,
prints the session header, the json event stream (the `EventSink` it hands pa-core is
stdout) and the auth notices, and renders the terminal selection to
stdout/stderr/exit code (mirroring modes/print-mode.ts). pa-core's `PrintRun` owns the
drive: prompt admission with the four session slash commands, the turn-boundary
compaction arms and compact-trigger auto-refine, the goal and autonomous continuation
loops, and the RLM quiescence drive over the inline subagents (the #2270 fix); see
pa-core's README.
The json stream emits the TS session-event surface byte-for-shape: the
session header row (version 3), the `message_update` streaming deltas with
the slim `assistantMessageEvent` (the daemon wire drops the nested
`partial`), `tool_execution_update`, the harness digest's message pair at
the first-turn boundary, the compaction `compaction_start`/`compaction_end`
pairs with their durable outcome rows, the `goal_update`/`session_action_update`
frames of the goal loop, and the refinement rows with
`refine_complete`/`refine_failed`. Verifiers: the `print_runtime_e2e`,
`print_inline_subagents_e2e` and `eval_composition_e2e` rows.

## Daemon client
The daemon-backed public commands (`list`, `stop`, `rename`, `send`, `schedule`) talk to the
pa-daemon supervisor over its JSONL Unix socket through the crate-private client module
(`daemon_client.rs`). The client never spawns a daemon - the TS CLI only auto-starts one for
the internal `daemon start`/`open` commands, which are not reachable from the public surface.

## Packaged layout / kernel packaging

The release artifact is exe-adjacent (TS install.sh native layout): the binary
plus `package.json` (the version manifest `--version` reads at runtime, with
the compiled-in version as the fallback), `prime-agent-runtime/` (the kernel
sidecar), `skills/`, and `docs/`. `scripts/package_release.py` (`make package`)
assembles, validates, version-pins, hashes, and tars it; the hidden
`--prime-agent-bootstrap` flag is the installer handoff that pre-bootstraps the
kernel venv. Verifier: `tests/packaged_layout_e2e.rs`.

A binary without that layout (`cargo install`, `cargo run`) carries the
runtime and skills it was built from: `pa-core`'s `build.rs` embeds
`prime-agent-runtime/` and `skills/` (the packer's content policy), and the
first use extracts them beside the kernel venv, `~/.prime/agent/runtime/<content hash>/` (atomic,
concurrent-safe, old bundles pruned conservatively). The live source checkout
is never the source, so an installed binary's kernel never runs a runtime the
binary cannot serve; installing a newer or older binary moves the kernel venv
with it. `PRIME_AGENT_RUNTIME_SOURCE` and `PI_PACKAGE_DIR` stay explicit
overrides.

## Daemon discovery
The discovery commands (`status`, `doctor [--fix]`, `shutdown [--force]`, TS
`cli/daemon-ps.ts`) live in the crate-private `daemon_discovery` module: an OS census of
listening unix sockets owned by product processes, a socket-dir sweep, probing/classifying
each discovered daemon, and the reap/shutdown planners and executors. Containment is part of
the contract: every scan, probe, and stop is scoped to an explicit `DaemonStateRoot`
(the env-resolved current root for the CLI); under a test harness a never-touch list also
refuses this machine's live daemon dirs, while the shipped binary always manages its own
default socket dir. All e2e daemons live in
test-created fixture directories only.

## Update flow (staged activation)

`prime-agent update` (the `update_flow` module): the invoking CLI plans, downloads, and
stages the candidate (`Acquire`..`Staged`), then spawns the detached
coordinator - the new binary running from its release dir - which adopts the
status file and owns the FSM to a terminal state (`Preparing..Complete`,
`Rollback` first-class). `intent.json` is the coordinator lock (Join on a
live holder, dead-holder steal), the status file keeps the TS schema plus
`updateId`/`state`/`epoch`, and the activation owns the bin-symlink swap
(`bin/previous` rollback pointer, `.activation-state` recovery record).
Mechanism support (version/channel policy, install-root layout, manifest
fetch, archive staging) lives in pa-core's `update` module; the daemon-side
prepare/commit/stop drivers and the boot restore pass are pa-daemon
(slices 2-3 and 5): the coordinator's `Restoring` phase polls the
successor's `update_restore_status` RPC for the real per-session counts.
The legacy restart adapter accepts the fresh status path supplied by shipped
TypeScript updaters after they activate a release. It reports the original
`requestId`/`phase` status schema, translates their session checkpoint to a
Rust roster, and restarts the daemon without repeating activation. Recovery
rosters remain on disk when restoration fails. Socket-scoped checkpoints resume
after an interrupted handoff when no daemon is running, and reject a different
installed version. The adapter holds the shipped TypeScript shutdown-admission
lease in both historic supervisor registries until Rust owns the replacement
socket, preventing old clients from launching a competing daemon. Admission
records are refreshed and removed only while their ownership token matches.
