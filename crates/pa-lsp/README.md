# pa-lsp

Claude Code's LSP integration for the kernel harness: the host half of `rlm.lsp.*` and Claude Code's
`<new-diagnostics>` reminders. Language servers come from configuration, start lazily per language in the session
worker, and shut down with their session. Never on the TUI's paint path.

Wired by `pa-cli` behind `feature = "lsp"` (default on). Inert without configured servers: no process starts, no
reminder is added. `--no-default-features` registers nothing: the kernel's `rlm.lsp.*` fail with the standard
unregistered-request error.

## Configuration

Claude Code's server shape (its plugins' `.lsp.json` / `lspServers`), keyed by server name:

```json
{
  "rust": {
    "command": "rust-analyzer",
    "args": [],
    "extensionToLanguage": { ".rs": "rust" },
    "env": {},
    "initializationOptions": {},
    "settings": { "rust-analyzer": { "check": { "command": "clippy" } } },
    "workspaceFolder": ".",
    "startupTimeout": 30000,
    "shutdownTimeout": 5000,
    "requestTimeout": 60000,
    "restartOnCrash": true,
    "maxRestarts": 3,
    "diagnostics": true
  }
}
```

- Sources: the `lspServers` setting (user `settings.json`, or a trusted project's `.prime/agent/settings.json`), then
  a project `.lsp.json` in the session's directory. A name in both comes from the setting. `.lsp.json` runs commands,
  so it is read only in a trusted workspace; `pa-cli` registers it with
  `pa_core::workspace_trust::register_gated_project_file`, so the trust prompt names it and a change asks again.
- Validation (Claude Code's): `command` without spaces unless absolute, at least one `extensionToLanguage` entry;
  `transport: "socket"` is refused (Claude Code's client speaks stdio only too). Bad entries are reported by
  `rlm.lsp.servers()` and skipped. The first server that maps an extension owns it.
- `settings` answers `workspace/configuration` (dotted sections) and is pushed with
  `workspace/didChangeConfiguration` after startup. `diagnostics: false` keeps navigation but never injects that
  server's diagnostics.
- `lspDiagnosticsWaitMs` (default 2000, at most 30000): how long the run's next boundary waits for the diagnostics of
  the files a cell wrote.

## Scope

- **Client** (`rpc`): JSON-RPC 2.0 over stdio with `Content-Length` frames; the server's own requests are answered
  (`workspace/configuration` from `settings`; capability registration, progress and message requests with null;
  `workspace/applyEdit` refused), `$/cancelRequest` after a timeout. Processes are spawned with `kill_on_drop`.
- **Servers** (`server`): Claude Code's `initialize` (client capabilities, workspace folder, `initializationOptions`),
  `initialized`, the startup and request timeouts (default 60 s each), `ContentModified` retried three times
  (500 ms, 1 s, 2 s), `shutdown` + `exit` within `shutdownTimeout` (default 5 s) and then a kill. Crash recovery is
  bounded for the session: every crash and failed start counts against `maxRestarts` (default 3; Claude Code resets
  its count after each successful start, so a server that crashes on every request restarts forever there);
  `restartOnCrash: false` gives up at the first crash.
- **Documents** (`manager`): a file is opened (`didOpen`) when a request or a write first touches it, sent again in
  full (`didChange`) when its content differs from what the server has, saved (`didSave`) after a write; at most 50
  stay open (least recently used closed with `didClose`). Files over 10 MB are refused, as in Claude Code. Unlike
  Claude Code, a request re-sends a document that changed on disk (a `bash()` command can rewrite files the kernel
  never reports).
- **Diagnostics** (`diagnostics`): Claude Code's registry: a publish with diagnostics is pending; a delivery
  deduplicates (within itself and against what was delivered for the file, remembered for 500 files), sorts each
  file's by severity, caps at 10 per file and 30 in all; a write forgets the file's delivered and pending ones; a
  publish older than the document's version is dropped. The text is Claude Code's, inside a `<system-reminder>`:

  ```
  <new-diagnostics>The following new diagnostic issues were detected:

  a.ts:
    ✘ [Line 1:5] Cannot find name 'x'. [2304] (typescript)</new-diagnostics>
  ```

  Delivery points: after a tool batch (`SessionFeature::after_tool_batch`, a hidden `feature_context` row joining the
  run's next model call; it waits up to `lspDiagnosticsWaitMs` for the written files' publishes) and ahead of the next
  prompt (`prompt_context`). Writes come from the `python_repl` result's `fileWrites` host fact (the kernel's
  `fileAccess` report: `open()` for writing, so `edit.run` and plain Python writes count, `bash()` commands do not)
  and the native `edit` tool's `path`.
- **Requests** (`requests`, `format`): Claude Code's LSP tool operations and result texts:

  | request | `rlm.lsp` | LSP method |
  |---|---|---|
  | `lsp.definition` | `definition(path, line, col)` | `textDocument/definition` |
  | `lsp.implementation` | `implementation(path, line, col)` | `textDocument/implementation` |
  | `lsp.references` | `references(path, line, col, *, include_declaration=True)` | `textDocument/references` |
  | `lsp.hover` | `hover(path, line, col)` | `textDocument/hover` |
  | `lsp.document_symbols` | `document_symbols(path)` | `textDocument/documentSymbol` |
  | `lsp.workspace_symbols` | `workspace_symbols(query, path=None)` | `workspace/symbol` |
  | `lsp.call_hierarchy` | `call_hierarchy(path, line, col, direction)` | `prepareCallHierarchy`, then `callHierarchy/incomingCalls` / `outgoingCalls` |
  | `lsp.diagnostics` | `diagnostics(path)` | the file's latest `publishDiagnostics` |
  | `lsp.servers` | `servers()` | (configuration and status) |

  Positions are 1-based both ways. Replies carry `text` (Claude Code's), `items` (the results as data), `result_count`,
  `file_count`, `server`, and `available: false` with Claude Code's "No LSP server available for file type" text when
  no server handles the extension. Locations in git-ignored files are dropped (`git check-ignore`, as Claude Code
  does). `workspace_symbols` without `path` asks every running server. `diagnostics` waits (at least 5 s) for a
  publish when the file had none or just changed, and what it shows is not delivered again as a reminder.

## Non-goals

- Claude Code's plugin-provided servers (its plugin manifests' `lspServers`): this harness has no Claude Code plugin
  loader; the setting and `.lsp.json` take the same entries.
- The socket transport, rename/format/code actions, IDE (MCP) diagnostics, one server set shared across sessions
  (each session, child sessions included, starts its own servers on first use).

## Public API

`LspFeature` (`new`, `with_loader`), `SESSION_EVENT`, and the modules `config` (`LspConfig`, `ServerConfig`,
`PROJECT_CONFIG_FILE`, `SETTINGS_KEY`, `DIAGNOSTICS_WAIT_KEY`), `diagnostics`, `format`, `manager`, `protocol`,
`requests` (`REQUEST_TYPES`), `rpc`, `server`.

## Seams

- `SessionFeature::register_host_handlers` (`lsp.*`), `after_tool_call` (written files), `after_tool_batch` and
  `prompt_context` (the reminder), `on_session_end` (shutdown and the adoption event).
- `pa_core::workspace_trust::register_gated_project_file` (`.lsp.json`, registered by `pa-cli`).
- The `python_repl` tool's `fileWrites` host fact.

## Dependencies

No LSP type crate: `lsp-types` (latest 0.97.0, from 2024) would add `bitflags` 1 and `fluent-uri` and parses
strictly, where Claude Code drops a malformed entry and keeps the rest; `protocol` parses the dozen shapes the client
reads tolerantly. `url` (already in the lock) converts paths and `file://` URIs.

## Files

None written. Settings and `.lsp.json` are read on a session's first LSP use.

## Telemetry

- `lsp_session` (catalog v6), when a session that started a server, made an `rlm.lsp` call or got a delivery ends:
  `servers_configured`, `servers_started`, `requests`, `request_errors`, `diagnostic_deliveries`,
  `diagnostics_delivered`, `server_failures`. Never a server name or command, a path, a symbol or a diagnostic.

## Tests

`tests/e2e` runs a fake language server (the test binary re-executed with `PA_LSP_FAKE_SERVER=1`, a toy language in
`.fake` files): every operation's text, diagnostics delivery and deduplication, crash recovery bounds,
`ContentModified` retries, `workspace/configuration`, and shutdown at the session's end. No real language server is
needed.
