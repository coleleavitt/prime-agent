# Fork feature crates

The fork's features (ported from its TypeScript product) live in **separate, removable crates**. Building
`prime-agent` without them yields upstream's native product, code path for code path. This document is the contract
every feature crate follows. `AGENTS.md` (crate ownership, dependency direction, gates) still applies in full.

## Rules

1. **One feature, one crate.** Name `pa-<feature>` (`pa-trace`, `pa-recall`, `pa-toolforge`, `pa-dream`,
   `pa-workflow`, `pa-ledger`, `pa-ravo`, `pa-learning`). The crate's `README.md` states scope, non-goals, public API,
   the seams it plugs into, the files it owns, and its telemetry events.
2. **Dependencies point down only.** A feature crate may depend on `pa-types`, `pa-telemetry`, `pa-agent`, `pa-ai`,
   `pa-models`, `pa-core`, and on other feature crates listed as its prerequisites. It never depends on `pa-daemon`,
   `pa-tui`, or `pa-cli`. **No native crate depends on a feature crate** — `pa-types`, `pa-core`, `pa-daemon`,
   `pa-tui` stay unaware of them.
3. **Composition happens in `pa-cli` only**, behind one Cargo feature per crate (`feature = "trace"`, `"recall"`, …).
   The fork build enables them through `pa-cli`'s `default` feature set; `cargo build -p pa-cli
   --no-default-features` is the native product and must always build and pass its tests. `#[cfg(feature = …)]`
   appears only in `pa-cli`.
4. **Plug in through generic seams, never feature-specific hooks in native code.** If a seam is missing, add a
   *generic* one to the native crate (a trait or registry that names no feature), in its own commit, with a test using
   a stub provider. Existing seams:
   - kernel host requests: `pa_core::kernel::shared::HostRequestHandlers` via `SessionEngineConfig::extra_host_handlers`;
   - observability: `tracing` spans/events emitted by native code; a feature crate supplies a `tracing_subscriber` Layer
     that `pa-cli` installs; an event under `pa_types::trace_context::SPAN_ATTRIBUTES_TARGET` adds its fields to the
     attributes of the span it happens in (how a feature annotates a native span such as `tool.execute`);
   - CLI commands: `#[cfg(feature)]` entries in `pa-cli`'s command registry dispatching into the feature crate;
   - session lifecycle: `pa_core::features::SessionFeature` default methods `on_session_start` (once, with the
     resumed history), `before_tool_call` / `after_tool_call` (observe a tool call and its result: content, details,
     and the tool's non-persisted `host_facts` such as an `ipython` cell's finished `bash()` commands; returned text is
     appended to the result the model sees), `on_message_end` (every finalized message, in order, non-blocking),
     `on_agent_end` (every run end, non-blocking), and `flush` (bounded, once at process exit); the context carries the
     session's artifact dir. Installed only when a feature is.
   Seams to add as features need them: system-prompt layer providers, turn-start observers.
5. **Data ownership.** A crate owns its files under `~/.prime/agent/<feature>/` (or the session artifact dir) and its
   key in `harness_state.json` (`HarnessState::extensions["<feature>"]`). It never rewrites another crate's data.
   Formats stay byte-compatible with the fork's TS files where those already exist on disk.
6. **Off means absent.** With the feature disabled: no files written, no host requests registered (the kernel's
   `rlm.<feature>` calls fail with the standard unregistered-request error), no prompt text, no telemetry, no added
   startup latency. Enabled features keep the paint path free of network and disk sync (`AGENTS.md` → Performance).
7. **Telemetry.** Each user-visible feature ships its adoption event in `pa-telemetry`'s schema in the same change.
8. **Parity reference** is the fork's TS branch `perf/session-catalog-resume` (these features never existed in
   upstream `v0.9.8`); `docs/*.md` from the TS era are the behavioural spec.

## Gates for every feature crate

```sh
cargo +1.98.1 clippy --workspace --all-targets --locked -- -D warnings          # all features (pa-cli default)
cargo +1.98.1 clippy -p pa-cli --all-targets --locked --no-default-features -- -D warnings   # native product
cargo +1.98.1 test --workspace --locked
cargo +1.98.1 test -p pa-cli --locked --no-default-features
```

plus the windows-gnu clippy lane, `cargo deny` for any new dependency, and the crate's own red-first tests.

## Port order

| phase | crates | depends on |
|---|---|---|
| 0 | native bug fixes (no crate) | — |
| 1 | `pa-trace` (span recorder → `agent.jsonl`, `TRACEPARENT` propagation, `prime-agent trace` / `health`, optional OTLP) | — |
| 2 | `pa-recall`, `pa-toolforge`, `pa-dream` (standalone zero-token runner), `pa-workflow` (v1) | `pa-trace` (spans) |
| 3 | `pa-ledger` (failure ledger, resolution index) → `pa-ravo` (gate, referee, trust, run service) | `pa-trace`, `pa-ledger` |
| 4 | `pa-learning` (learning index, trajectory index) → in-session Dream in `pa-dream` | `pa-trace`, `pa-ravo` |
| 5 | Workflow V2 in `pa-workflow`, persisted session-catalog index, Mermaid rendering | per item |

The inventory behind this table (sizes, TS paths, key commits) is kept with the porting work.
