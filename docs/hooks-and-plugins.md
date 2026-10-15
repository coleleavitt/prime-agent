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
| tool-call policy | `review_tool_call` -> `FeatureVerdict` | `Block(reason)` refuses the call; the model sees the reason as the tool's error result; `Ask(reason)` asks the user |
| kernel action policy | `review_host_request` -> `FeatureVerdict` | gates the kernel's host requests (`bash.run`, `computer_use.*`, `mcp.*`, ...) before they run: a refused `bash()` raises in Python with the reason |
| kernel action outcome | `after_host_request` -> note | sees each request's reply (a `bash.run`/`bash.follow` reply carries the command's `finished` event); a note reaches the model on the cell's result |
| prompt context | `prompt_context` -> `PromptDecision` | `Context(text)` adds a hidden row ahead of the user's prompt; `Block(reason)` refuses the prompt |
| status panel | `publish_feature_status` (existing) | `status.panel` (an array of strings) renders under the chat editor; `line` stays the agents-view summary |
| confirmation | `features::confirm` | puts an `Ask` to the user (below) |
| slash commands | `slash_commands` / `execute_slash_command` (existing) | `/wasm` |

Verdicts compose in install order: the first `Block` wins, then any `Ask` asks the user (one question carrying every
asking feature's reason), else the action runs. Plan mode's own gate runs ahead of every feature. A feature that
fails or times out fails **open** for observation and **closed** for policy only if it says so (`pa-hooks`: Claude
Code's exit-code 2 and `permissionDecision: "deny"`; `pa-plugins`: a trap in `review-*` blocks).

The kernel's reads and cleanup of its own jobs (`bash.follow`, `bash.output`, `bash.kill`, ...) and its bookkeeping
(`bash.consumed`, `model.info`, `telemetry.emit`) are not reviewed (`features::is_reviewed_host_request`), and the
bookkeeping is not observed either: `bash.consumed` must stay synchronous. The kernel's first `bash()` in a new
environment is answered `EnvUnknown` and resent with the environment attached; the policy treats the resend as the
same action (not reviewed or asked again) and hides the `EnvUnknown` reply from observers.

Notes from `after_host_request` wait in the session's `FeatureNotes`: the after hook of the tool call whose cell sent
the request appends them to its result; notes from a request outside any tool call ride the next admitted prompt's
hidden context row.

## Asking the user

A `FeatureVerdict::Ask(reason)` holds the action and publishes the question as the session's `confirmation` feature
status (`{"confirmations": [{"id", "action", "reason"}, ...]}`). The action text is the tool and its arguments, or
for `bash.run` the command (never its environment), clipped to 600 characters.

- **TUI:** the oldest pending question opens the yes/no confirm panel ("Allow this action?") with **No** selected,
  so a keystroke meant for the editor never allows an action. Yes, No and Escape answer with
  `answer_feature_confirmation { activeSessionId, confirmationId, allow }`, a daemon command advertised by the
  `feature_confirmation` capability (additive; no protocol version change). A client attaching later sees pending
  questions on the roster summary; a question answered elsewhere closes its panel.
- **Daemon:** a session whose engine has a feature-status sink (every daemon session) has a confirmation surface.
  An unanswered question is refused after `CONFIRMATION_TIMEOUT` (5 minutes); an aborted run stops waiting.
- **Print mode:** there is no surface, so an `Ask` is refused at once: `<reason> (it needs the user's confirmation,
  and no client can ask for it)`. A refused `Ask` reads `<reason> (the user refused it)`.

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
- **Tool names:** the kernel's `bash()` calls are presented as `Bash` with `tool_input.command`, and its MCP calls
  as `mcp__<server>__<tool>`, so existing Claude Code guard scripts work unmodified. The model's own tool calls keep
  their names (`python_repl`, ...).
- **`PostToolUse` for `Bash`** fires once the command's result arrives (in its `bash.run` reply, or a later
  `bash.follow` for the same job), with Claude Code's `tool_response` (`stdout`, `stderr` (empty: the kernel's
  commands write one merged stream), `interrupted`, `isImage`) plus `exit_code`. A large result the kernel spilled to
  a file is read back, capped at 64 KiB. Its feedback reaches the model on the cell's result as
  ``Hook feedback on `<command>`: <reason>``. MCP calls get `PostToolUse` with the reply as `tool_response`.
- **Decisions:**
  - Exit code 2 blocks, with stderr as the reason.
  - JSON `{"decision": "block", "reason"}` blocks.
  - `hookSpecificOutput.permissionDecision` of `deny` blocks; `allow` passes; `ask` asks the user, with
    `permissionDecisionReason` (see Asking the user).
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
  `crates/pa-plugins/wit/plugin.wit` (package `prime-agent:plugin@0.2.0`). Any language with component tooling
  works (Rust `wit-bindgen` with the `wasm32-wasip2` target, JavaScript `jco componentize`, Python
  `componentize-py`, C `wit-bindgen c`). A worked Rust example is `examples/plugins/command-guard`.
- **Exports:** `metadata` (name, description, tools, commands), `on-session-start`, `on-agent-end`,
  `review-tool-call`, `after-tool-call`, `review-host-request`, `after-host-request`, `prompt-context`, `call`
  (kernel-invokable methods, the declared tools among them), `run-command`.
- **Verdicts:** `allow`, `ask(reason)` (see Asking the user), `block(reason)`.
- **Imports (host):** `log` and `set-status` (the panel lines and agents-view line).
- **Tools:** each `tool-spec` (name, description, `parameters-json`: a JSON Schema for the arguments object) is a
  method of `call` the model is told about: the session's first admitted prompt lists every plugin's tools with
  their signatures (`notes: search(query: string, limit?: integer) - ...`). The model calls one with
  `await wasm_plugins.call("notes", "search", query=...)`; the host validates the arguments against the schema
  (pa-agent's tool-argument validator) before the plugin sees them. `await wasm_plugins.tools()` lists them again.
  Model tools stay kernel functions, not provider tool definitions: the kernel is the tool surface.
- **Slash commands:** each `command-spec` (name, description, optional argument hint) runs as
  `/wasm <plugin> <command> [args]` through the plugin's `run-command` export; its `ok` text is the command's
  result row. `/wasm` lists every plugin's commands, `/wasm <plugin>` one plugin's. (`/plugins` is upstream's MCP
  catalog.)
- **Names:** tool and command names are ASCII letters, digits, `_`, `-`, `.`, unique per plugin; a malformed
  declaration fails the plugin's load.

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

**Kernel access:** the bundled `wasm-plugins` skill (`await wasm_plugins.tools()`, `await wasm_plugins.list()`,
`await wasm_plugins.call(name, method, **data)`) sends the `plugin.list` and `plugin.call` host requests; a
plugin's `call` export answers with JSON.

**Writing one in Rust** (`examples/plugins/command-guard`): a `cdylib` crate with `wit-bindgen`'s `generate!` over
`crates/pa-plugins/wit`, an `impl Guest`, and `export!`. Build with
`cargo build --release --target wasm32-wasip2` (needs `rustup target add wasm32-wasip2`; the target links the
component directly) and copy the `.wasm` into `~/.prime/agent/plugins/`. The example's `check.sh` builds it and runs
the host test that loads it (`crates/pa-plugins/tests/example_plugin.rs`).

## Status panel (`pa-tui`)

The daemon already keeps each feature status on the session's roster summary (`featureStatus.<feature>`) and pushes a
`feature_status` session event on every change (`pa-daemon/src/feature_status.rs`, additive on the wire). The chat
view seeds its panel from the attached session's summary, which covers a status published before the client
attached, and patches it from the events. Every non-empty `status.panel` renders directly under the editor, ordered
by feature name, one muted row per line, truncated to the width and capped at 6 rows (a longer set ends with
`+N more`). A status published while the session was still being built is held until the session's sink registers
(`pa_core::features::publish_feature_status`). Older clients ignore the key; no protocol version changes.

## Not covered

- Network access for plugins.
- Provider-level tool definitions from plugins: plugin tools are kernel calls (the kernel is the tool surface).
- Top-level slash command names per plugin: plugin commands run under `/wasm` (session commands are a static
  registry shared by every client).
- After compaction the model no longer sees the first prompt's tool list; `wasm_plugins.tools()` (the skill's
  documented entry point) lists them again.
