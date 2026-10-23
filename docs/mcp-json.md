# Project MCP servers (`.mcp.json`)

Prime Agent reads MCP servers the way Claude Code declares them, beside its own
`mcpServers` setting. A repository can ship a `.mcp.json`; its servers are
listed at once but start only after you approve them.

## Where servers come from

| scope | file | needs approval |
|---|---|---|
| `settings` | `~/.prime/agent/settings.json`, and the project's `.prime/agent/settings.json` once the workspace is trusted | no |
| `local` | `~/.claude.json`, `projects["<repo root>"].mcpServers` (opt-in) | no |
| `project` | every `.mcp.json` from the filesystem root down to the working directory | yes |
| `user` | `~/.claude.json`, `mcpServers` (opt-in) | no |

A name declared in several places takes the first scope in that order (Claude
Code's local > project > user, under Prime Agent's own settings). Of several
`.mcp.json` files, the one nearest the working directory wins. The
`~/.claude.json` scopes (`$CLAUDE_CONFIG_DIR/.claude.json` when set) load only
with the opt-in the permission rules use, in `~/.prime/agent/settings.json`:

```json
{ "permissions": { "claudeCodeSettings": true } }
```

## Entry format

Claude Code's: a stdio server is `{"command", "args", "env"}` (`"type":
"stdio"` optional); a remote one is `{"type": "http", "url", "headers"}`
(`"streamable-http"` is the same; an `oauth` object turns on the OAuth login).
`sse` and `ws` servers are skipped with a warning: Prime Agent speaks
streamable HTTP and stdio. Values expand `${VAR}` and `${VAR:-default}` when
the file loads: a stdio server's command, args and env, a remote server's URL
and headers. Variables come from the host environment under the
`kernel.environment` policy; an unset variable without a default stays as
written and is reported ("Missing environment variables: ..."). A model-provider
API key never expands into a remote server's URL or headers (it reads as empty,
with a warning).

Problems are reported with Claude Code's wording: a file that is not JSON,
`servers` where `mcpServers` belongs, an entry with a `url` but no `type`, an
unknown type, a wrong-typed field, values with stray whitespace. A bad entry is
skipped; the rest of the file loads. `prime-agent mcp list` and the
`get_mcp_connections` response (`configIssues`) show them.

## Approval

A project server is rejected when its name is in `disabledMcpjsonServers`,
approved when it is in `enabledMcpjsonServers` or `enableAllProjectMcpServers`
is `true`, and pending otherwise. Names compare as Claude Code compares them
(any character outside `[A-Za-z0-9_-]` reads as `_`). The keys count from:

- `~/.prime/agent/settings.json`;
- the project's `.prime/agent/settings.json`, only once the workspace is
  trusted (a repository cannot approve its own servers);
- the answers you gave, kept per workspace in
  `~/.prime/agent/mcp-project-choices.json`;
- with the opt-in, Claude Code's `~/.claude/settings.json`, and the project's
  `.claude/settings.json` and `.claude/settings.local.json` once the workspace
  is explicitly trusted.

A pending server is shown in `/mcp` as "Pending approval" and never started;
the kernel's `rlm.mcp` calls to it fail with "approve it via /mcp first". When
an interactive session starts (and whenever `/mcp` opens) the confirm panel
asks about each pending server, showing what it runs: Yes uses it, "Yes, and
don't ask again" uses it and every future server of this project, No rejects
it. An unanswered question leaves the server pending. Print mode never asks.
`prime-agent mcp reset-project-choices` forgets this project's recorded
answers.

## Commands

- `prime-agent mcp add <name> --scope project ...` writes `./.mcp.json` in
  Claude Code's shape (`--env CHILD=SOURCE` becomes `"CHILD": "${SOURCE}"`,
  `--bearer-token-env-var V` an `Authorization: Bearer ${V}` header). A
  symlinked `.mcp.json` is never written. `--scope user` (the default) writes
  the settings.
- `prime-agent mcp remove <name> --scope project` removes it again.
- `prime-agent mcp list` adds the `.mcp.json` and Claude Code servers, with
  their scope, file and approval.

## Not ported

Claude Code's managed and enterprise MCP files, plugin-provided servers,
`headersHelper` (ignored with a warning), the multi-select dialog (each pending
server is asked about in turn), and its auto-approval in `-p` mode and in
bypass-permissions mode: Prime Agent's print mode leaves pending servers
pending.
