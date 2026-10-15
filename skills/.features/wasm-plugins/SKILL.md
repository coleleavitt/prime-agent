---
name: wasm-plugins
description: Call the user's WebAssembly plugins' tools from the Python REPL - list the loaded plugins and their tools, and call a tool with JSON arguments. Use when a task names a plugin or needs what one provides (its own data, lookups, checks). Plugins also review tool calls, bash commands and prompts on their own; a refusal arrives as an error naming the plugin.
---

# Plugins

The user's plugins are sandboxed WebAssembly components the host loads from
`~/.prime/agent/plugins/` (and from the project's `.prime/agent/plugins/` in a
trusted workspace). Besides reviewing what the session does, a plugin may offer
tools: the session's first prompt lists them. Call them directly from the
Python REPL:

```python
await wasm_plugins.tools()
await wasm_plugins.call("notes", "search", query="deploy checklist")
```

## API

- `await wasm_plugins.tools()` - every loaded plugin's tools:
  `[{"plugin", "name", "description", "signature", "parameters"}, ...]`.
  `parameters` is the JSON Schema of the tool's arguments.
- `await wasm_plugins.list()` - the session's loaded plugins:
  `[{"name", "description", "path", "tools", "commands"}, ...]`.
- `await wasm_plugins.call(name, method, **data)` - call plugin `name`'s tool
  (or other method) `method` with `data` as a JSON object; returns the
  plugin's JSON answer. A declared tool's arguments are validated against its
  schema first. Raises when the plugin is not loaded, the arguments do not
  match, the plugin does not know the method, or it fails (a plugin runs
  under a deadline and a memory cap).

## Rules

- Read a tool's `signature` and `description` from `tools()` before calling
  it; do not guess method names.
- A plugin can refuse a tool call, a `bash()` command, or a prompt: the error
  names the plugin and its reason. Do not retry a refused action in another
  form to get around the refusal; tell the user what was refused and why.
- A plugin can also ask the user before an action runs; the call waits for
  their answer and raises if they refuse.
- Plugin commands (`/wasm <plugin> <command>`) are the user's to run, not
  callable from the REPL.
