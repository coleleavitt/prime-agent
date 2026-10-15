---
name: wasm-plugins
description: Call the user's WebAssembly plugins from the Python REPL - list the loaded plugins and invoke a plugin's methods with JSON data. Use when a task names a plugin or needs what one provides (its own data, lookups, checks). Plugins also review tool calls, bash commands and prompts on their own; a refusal arrives as an error naming the plugin.
---

# Plugins

The user's plugins are sandboxed WebAssembly components the host loads from
`~/.prime/agent/plugins/` (and from the project's `.prime/agent/plugins/` in a
trusted workspace). Besides reviewing what the session does, a plugin may offer
methods this skill calls. Call it directly from the Python REPL:

```python
await wasm_plugins.list()
await wasm_wasm_plugins.call("notes", "search", query="deploy checklist")
```

## API

- `await wasm_plugins.list()` - the session's loaded plugins:
  `[{"name", "description", "path"}, ...]`.
- `await wasm_plugins.call(name, method, **data)` - call plugin `name`'s `method`
  with `data` as a JSON object; returns the plugin's JSON answer. Raises when
  the plugin is not loaded, does not know the method, or fails (a plugin runs
  under a deadline and a memory cap).

## Rules

- A plugin knows its own methods; read its description from `list()` before
  guessing a method name.
- A plugin can refuse a tool call, a `bash()` command, or a prompt: the error
  names the plugin and its reason. Do not retry a refused action in another
  form to get around the refusal; tell the user what was refused and why.
