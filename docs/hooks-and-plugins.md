# Hooks and WebAssembly plugins

Upstream removed the TypeScript extension host in #3189 (`v0.10.0`); `extensions` in settings has been an inert key
since. Users asked for a replacement in discussions #3200 and #3484: status panels (Orca), a policy point on every
tool call (an IT team pinned to 0.9.8 for it), session and prompt events for memory integrations, and Claude
Code-compatible command hooks (#2004). This fork answers with two layers that share one set of generic seams:

| layer | crate | what a user writes | runs |
|---|---|---|---|
| command hooks | `pa-hooks` | a shell command in `settings.json` (`hooks`, Claude Code's schema) | a child process per event |
| WebAssembly plugins | `pa-plugins` | a WebAssembly component targeting `wit/plugin.wit` | in the session worker, sandboxed |

Both are fork feature crates (`docs/fork-feature-crates.md`): wired only in `pa-cli` behind the `hooks` and `plugins`
Cargo features, built on `pa_core::features::SessionFeature`. `--no-default-features` stays upstream's product.

## Seams (`pa-core`, generic, name no feature)

| seam | `SessionFeature` method | effect |
|---|---|---|
| tool-call policy | `review_tool_call` -> `FeatureVerdict` | `Block(reason)` refuses the call; the model sees the reason as the tool's error result |
| kernel action policy | `review_host_request` -> `FeatureVerdict` | gates the kernel's host requests (`bash.*`, `computer_use.*`, ...) before they run: a refused `bash()` raises in Python with the reason |
| prompt context | `prompt_context` -> `PromptDecision` | `Context(text)` adds a hidden row ahead of the user's prompt; `Block(reason)` refuses the prompt |
| status panel | `publish_feature_status` (existing) | `status.panel` (an array of strings) renders under the chat editor; `line` stays the agents-view summary |

Verdicts compose in install order, and the first `Block` wins. Plan mode's own gate runs ahead of every feature. A
feature that fails or times out fails **open** for observation and **closed** for policy only if it says so
(`pa-hooks`: Claude Code's exit-code 2 and `permissionDecision: "deny"`; `pa-plugins`: a trap in `review-*` blocks).

The model-facing surface is the Python kernel: the model's tool call is nearly always `python_repl`, and the
commands that matter run as `bash()` inside it. A policy that only sees tool calls would see one opaque cell.
`review_host_request` puts the policy point where the kernel's actions reach the host, so a `bash()` call is
reviewed as the command it runs.

## Command hooks (`pa-hooks`)

Settings key `hooks`, Claude Code's shape:

```json
{ "hooks": {
    "SessionStart":     [{ "hooks": [{ "type": "command", "command": "cat ~/.prime/brief.md" }] }],
    "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "~/bin/memory-recall" }] }],
    "PreToolUse":       [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/bin/guard", "timeout": 10 }] }],
    "PostToolUse":      [{ "matcher": "python_repl|Bash", "hooks": [{ "type": "command", "command": "~/bin/audit" }] }],
    "Stop":             [{ "hooks": [{ "type": "command", "command": "~/bin/notify" }] }]
} }
```

- **Events:** `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`. The JSON on stdin is
  Claude Code's: `hook_event_name`, `session_id`, `cwd`, `transcript_path`, `tool_name`, `tool_input`,
  `tool_response`, `prompt`.
- **Tool names:** the kernel's `bash()` calls are presented as `Bash` with `tool_input.command`, so existing Claude
  Code guard scripts work unmodified. The model's own tool calls keep their names (`python_repl`, ...).
- **Decisions:**
  - Exit code 2 blocks, with stderr as the reason.
  - JSON `{"decision": "block", "reason"}` blocks.
  - `hookSpecificOutput.permissionDecision` of `deny` blocks; `allow` passes.
  - `ask` blocks with the reason "needs confirmation". There is no interactive confirmation surface, and a
    policy hook must not fail open.
  - `hookSpecificOutput.additionalContext`, or plain stdout from `SessionStart`/`UserPromptSubmit`, becomes
    hidden context.
- **Matchers** are regular expressions over the tool name, and an empty matcher matches every tool.
- **Defaults:** `timeout` is in seconds (default 60). A timeout or spawn failure is reported and treated as
  "no decision".
- **Scope:** user `~/.prime/agent/settings.json`, and project `.prime/agent/settings.json` only in a trusted
  workspace (`hooks` is a gated settings key). Project hooks run after user hooks.

## WebAssembly plugins (`pa-plugins`)

- **Where:** `~/.prime/agent/plugins/<name>.wasm`, and `<project>/.prime/agent/plugins/<name>.wasm` in a trusted
  workspace (the directory is part of the workspace trust fingerprint: a new or changed plugin asks again). An
  optional `<name>.toml` manifest sits beside each.
- **Format:** a WebAssembly **component** (component model, WASI preview 2) implementing the `plugin` world in
  `crates/pa-plugins/wit/plugin.wit`. Any language with component tooling works (Rust `cargo component` or
  `wit-bindgen`, JavaScript `jco componentize`, Python `componentize-py`, C `wit-bindgen c`).
- **Exports:** `metadata`, `on-session-start`, `on-agent-end`, `review-tool-call`, `after-tool-call`,
  `review-host-request`, `prompt-context`, `call` (kernel-invokable methods).
- **Imports (host):** `log` and `set-status` (the panel lines and agents-view line).

**Sandbox** (wasmtime 49):
- WASI starts deny-all: no preopened directories, no environment, no arguments, stdin closed, no network
  addresses, no name lookup.
- The manifest may grant read-only or read-write directories and pass host environment variables by name; an
  unknown key refuses the manifest (and the plugin). It cannot grant network access:

  ```toml
  timeout_ms = 2000      # per call, at most 60000
  memory_mib = 64        # linear memory, at most 4096
  env = ["NOTES_TOKEN"]
  [[dirs]]
  host = "notes"         # relative to the plugin's directory, or absolute
  guest = "/notes"
  write = false
  ```
- Every call runs under an epoch deadline (default 2000 ms, manifest `timeout_ms`) and a linear-memory cap (default
  64 MiB, manifest `memory_mib`).
- A trap or deadline reports the plugin's error. A `review-*` trap blocks, so a policy plugin fails closed, and
  every other export fails open.
- One `Store` per plugin per session, behind a mutex. Calls run on a blocking thread; the engine and the compiled
  component are shared across sessions.

**Kernel access:** the bundled `wasm-plugins` skill (`await wasm_plugins.list()`, `await wasm_plugins.call(name, method, **data)`) sends
the `plugin.call` host request; a plugin's `call` export answers it with JSON.

## Status panel (`pa-tui`)

The daemon already keeps each feature status on the session's roster summary (`featureStatus.<feature>`) and pushes a
`feature_status` session event on every change (`pa-daemon/src/feature_status.rs`, additive on the wire). The chat
view seeds its panel from the attached session's summary, which covers a status published before the client
attached, and patches it from the events. Every non-empty `status.panel` renders directly under the editor, ordered
by feature name, one muted row per line, truncated to the width and capped at 6 rows (a longer set ends with
`+N more`). A status published while the session was still being built is held until the session's sink registers
(`pa_core::features::publish_feature_status`). Older clients ignore the key; no protocol version changes.

## Not covered

- Interactive confirmation (`ask`): needs a client confirmation surface on the daemon wire.
- Plugin-registered model tools and slash commands: the kernel is the tool surface; a plugin reaches the model
  through the `wasm-plugins` skill.
- Network access for plugins.
