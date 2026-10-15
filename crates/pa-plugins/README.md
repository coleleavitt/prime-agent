# pa-plugins

WebAssembly plugins for Prime Agent: sandboxed components the user drops into a directory, which review what a
session does and extend it. The design and the user-facing contract are in `docs/hooks-and-plugins.md`.

## Scope

- Load `<agent dir>/plugins/*.wasm`, then (in a workspace the user trusted) `<project>/.prime/agent/plugins/*.wasm`,
  each a component implementing the `plugin` world in `wit/plugin.wit`, with an optional `<name>.toml` manifest.
- Per session, per plugin: one wasmtime `Store` and instance. Every call runs on a blocking thread under an epoch
  deadline (default 2000 ms, manifest `timeout_ms`, at most 60000) and a linear-memory cap (default 64 MiB, manifest
  `memory_mib`). A trap drops the instance; the next call instantiates a fresh one, which sees `on-session-start`
  again.
- WASI preview 2, deny-by-default: no preopens, no environment, no arguments, stdin closed, no network addresses, no
  name lookup. A manifest may grant directories (`[[dirs]] host`, `guest`, `write`) and pass host environment
  variables by name (`env`); it cannot grant network access.
- The session seams (`pa_core::features::SessionFeature`):
  - `review_tool_call` and `review_host_request`: a plugin's `block` refuses. A trap also refuses, so a policy
    plugin fails closed.
  - `prompt_context`: the plugin's context is added, and its `block` refuses the prompt.
  - `after_tool_call`: the plugin's text is appended to the result.
  - `on_session_start` and `on_agent_end`.
  - `register_host_handlers`: `plugin.list` and `plugin.call`.
  - The bundled `wasm-plugins` skill, visible only where a plugin file exists.
- The `host` import: `log` goes to `tracing` (target `pa_plugins`), and `set-status` publishes the feature status
  `plugin:<name>`. It sets `line` and `status.panel`, which the TUI renders under the chat editor. A status for any
  session other than the instance's own is refused.

## Non-goals

- Network access for plugins, and model tools or slash commands registered by plugins (the kernel is the tool
  surface; plugins reach the model through the `wasm-plugins` skill).
- Interactive confirmation of a refused action.
- Async host calls (`component-model-async`): calls are synchronous and bounded.

## Public API

- `PluginsFeature`: the `SessionFeature` `pa-cli` installs (feature `plugins`).
- `plugin_files(agent_dir, cwd)`: the files a session loads, in order.
- `SessionPlugin`: one loaded plugin in one session (`load`, `call`).
- `manifest::Manifest`: a plugin's resolved sandbox settings.
- `status_feature(name)`, and the constants `FEATURE_NAME`, `PLUGINS_SKILL`, `PLUGINS_DIR`, `CALL_REQUEST`,
  `LIST_REQUEST`.

## Seams it plugs into

- `pa_core::features` (the trait above, `publish_feature_status`).
- `pa_core::workspace_trust::register_gated_project_dir`: `pa-cli` gates `.prime/agent/plugins/`, so a project's
  plugins load only once the user trusts the workspace, and a changed plugin asks again.
- `skills/.features/wasm-plugins`: the kernel-side skill.

## Files it owns

None on disk. It reads the plugin directories and manifests; plugins write only where a manifest grants.

## Telemetry

None.

## Tests

- `src/tests.rs`: one instance over the fixture component. It covers every export, both host imports, the
  deadline, the memory cap, recovery after a trap, and project plugins behind trust.
- `tests/session.rs`: the feature in a real session (a refused prompt, a refused tool, the context row, the held
  status), and over a real kernel a refused destructive `bash()` and `wasm_plugins.call`.
- The fixture is hand-written against the canonical ABI (`tests/fixtures/fixture-plugin.core.wat`). `regenerate.sh`
  wraps it into the component text the tests load, so no wasm toolchain is needed.
