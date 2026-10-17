# pa-permissions

Claude Code's permission system for Prime Agent: `permissions.allow`, `ask` and `deny` rule lists in Claude Code's
syntax, permission modes, `additionalDirectories`, the `/permissions` command, a mode indicator and the shift+tab mode
cycle, and "always allow" answers that persist an allow rule. A fork feature crate (`docs/fork-feature-crates.md`),
wired in `pa-cli` behind the `permissions` Cargo feature, built on the `pa_core::features` policy seams.

**Default: nothing changes.** Without settings every action runs, as before. An action no rule covers runs unless the
user settings choose `"fallback": "ask"` (Claude Code's default).

## What the rules govern

The model's surface is the Python kernel, so rules apply where the kernel's actions reach the host:

| Claude Code tool | Prime Agent surface | rule content |
|---|---|---|
| `Bash` | the kernel's `bash()` (`bash.run`), the legacy `bash` tool | command prefix (`git:*`), wildcard (`npm run *`), exact |
| `Edit` (`Write`, `MultiEdit`, `NotebookEdit` read as `Edit`) | the bundled `edit` skill (`file.edit`, sent before it writes), the legacy `edit` tool, files a `bash()` command writes (redirections; `rm`, `mv`, `cp`, `touch`, `mkdir`, `ln`, `chmod`, `chown`, `truncate`, `tee`, `install`, `sed -i` operands) | gitignore-style path |
| `Read` (`Glob`, `Grep` read as `Read`) | files a `bash()` command reads (`<` redirections; `cat`, `head`, `tail`, `grep`, `rg`, `cp` sources, ... operands); `file.read` | gitignore-style path |
| `WebFetch` | `web.*` host requests carrying a `url` | `domain:<host>` (`*` wildcards) |
| `WebSearch` | `web.*` host requests carrying a `query` | `*` wildcard over the query |
| `mcp__<server>[__<tool>]` | `mcp.session.call_tool`, `mcp.integration.call_tool` | none (`mcp__server`, `mcp__server__*`, `mcp__server__tool`) |
| any other name | the model's tool calls by name (`python_repl`) | none |

**Python is not gated.** Code the kernel runs directly (`open()`, `subprocess`, `os`, `pathlib` writes) never reaches
the host and is not checked. The rules are a policy over the agent's tools, not a sandbox (see the OS sandbox and plan
mode for confinement).

## Rules

Claude Code's syntax and parser (`Tool`, `Tool(content)`, `\(`/`\)`/`\\` escapes; `Bash(*)` reads as `Bash`), its
validation errors and warnings, and its matching:

- **Precedence:** a deny rule refuses; else an ask rule asks (also in `bypassPermissions`); else the mode; else an
  allow rule allows; else read-only commands run; else the fallback. Sources do not outrank each other.
- **Compound commands** split on `&&`, `||`, `;`, `|`, `|&`, `&`, newlines and subshells (quote-aware): a deny or ask
  rule matching any part applies to the whole; an allow rule must cover every part (a prefix rule never approves a
  compound command whole). Command and process substitutions, backticks and heredocs make a command unverifiable: no
  allow rule approves it, and the substituted commands are checked as parts too. Deny and ask rules also see each part
  without leading `VAR=value` assignments and wrappers (`env`, `timeout`, `time`, `nice`, `nohup`, `stdbuf`) and through
  `xargs`; allow rules see through wrappers but not assignments.
- **Bash patterns:** `prefix:*` matches the prefix alone or followed by a space; an unescaped `*` matches anything, and a
  single trailing ` *` also matches the bare command (`git status *` covers `git status`); otherwise exact. Spaces and
  tabs are normalized.
- **Path patterns** (gitignore rules, a match covers its subtree): `//abs` from the root, `~/p` from home, `/p` from the
  settings root (the project for project files, the home for user files), `./p` from the working directory, `p` at any
  depth under it.

## Settings

The `permissions` key, Claude Code's shape, from (highest first): Claude Code managed settings (opt-in),
`.prime/agent/settings.local.json`, `.prime/agent/settings.json`, `~/.prime/agent/settings.json`, and with
`"claudeCodeSettings": true` in the user settings also `.claude/settings.local.json`, `.claude/settings.json` and
`~/.claude/settings.json` (`$CLAUDE_CONFIG_DIR`).

```json
{ "permissions": {
    "allow": ["Bash(npm run test:*)", "Edit(src/**)", "WebFetch(domain:docs.rs)"],
    "ask":   ["Bash(git push:*)"],
    "deny":  ["Read(./.env)", "Bash(rm -rf:*)", "mcp__github__delete_repo"],
    "defaultMode": "acceptEdits",
    "additionalDirectories": ["../shared"],
    "disableBypassPermissionsMode": "disable",
    "fallback": "ask",
    "claudeCodeSettings": true
} }
```

- `fallback` (fork-only, user settings only): `allow` (default) or `ask` for actions no rule covers (`Bash` that is not
  read-only, `Edit`, `WebFetch`, `WebSearch`, MCP calls).
- `claudeCodeSettings` (fork-only, user settings only): also read Claude Code's files. Off by default: their rules would
  start refusing or asking in sessions that never did (and print mode refuses every ask).
- **Trust:** a project-scoped file in an untrusted workspace contributes its `deny` and `ask` rules only; `allow`,
  `defaultMode` and `additionalDirectories` wait for trust. No project file may default to `bypassPermissions`.
- Managed `allowManagedPermissionRulesOnly: true` keeps only the managed rules.
- Settings re-read when a source file changes.

## Modes

| mode | effect |
|---|---|
| `default` | the rules and the fallback |
| `acceptEdits` | edits in the working directories (the session's and `additionalDirectories`) and the file commands `mkdir`, `touch`, `rm`, `rmdir`, `mv`, `cp`, `sed` on them run without asking |
| `plan` | edits are refused; commands other than read-only ones ask |
| `bypassPermissions` | everything runs except what a deny or ask rule covers; offered in the cycle only when the user settings default to it |
| `dontAsk` | what would ask is refused |

`/permissions mode <mode>` switches. The session's status (`permissions` feature status) carries the mode, an
indicator row under the editor (`▸▸ accept edits on (shift+tab to cycle)`, none in `default`), and `cycle`: the session
commands the TUI's plan key (shift+tab) sends next. The cycle is Claude Code's (default, acceptEdits, plan, bypass), and
entering or leaving `plan` also sends the native `/plan on` / `/plan off` (the kernel confinement). It is published
only where it differs from the plain plan toggle (the `ask` fallback, bypass enabled, or a non-default mode), so with no
settings shift+tab stays upstream's `/plan`.

## Asking

An ask from a rule joins the other features' asks (`FeatureVerdict::Ask`). An ask from the fallback or the mode is put
to the user by this crate with `pa_core::features::confirm_offering`, offering the allow rule that would stop it
(`Bash(npm test *)`, `Edit(./src/**)`, `WebFetch(domain:example.com)`, `mcp__s__t`): the TUI shows "Yes, and don't ask
again for <rule>", answered with `remember: true` on `answer_feature_confirmation`, and the rule is appended to
`.prime/agent/settings.local.json` (in an untrusted workspace, kept for the session). Print mode has no surface: every
ask is refused.

## `/permissions`

`/permissions` lists the mode, the fallback, the rules by behavior with their sources, the working directories, the
source files, and problems (skipped rules and keys). `mode [<mode>]`, `allow|ask|deny <rule>` (to the local file),
`remove <rule>`, `check <Tool(content)>` (what an action would get now).

## Non-goals

- Claude Code's `auto` mode (a classifier), sandbox settings, `Cd(...)` rules, managed-only policies beyond
  `allowManagedPermissionRulesOnly`, and the `/permissions` editor UI (the command is text).
- Tree-sitter-grade shell parsing: the splitter is quote-aware and conservative (anything it cannot verify is not
  approved by allow rules).

## Public API

`PermissionsFeature`, `FEATURE_NAME`, `resolve_sources`; modules `rule` (parse, print, validate), `bash` (patterns),
`shell` (splitting), `paths` (path patterns), `settings` (sources, layering, persistence), `decide` (`Policy`,
`Action`, `Decision`), `mode`, `state`, `command`, `actions`.

## Files and seams

Reads the settings files above; writes `.prime/agent/settings.local.json`. Seams: `review_host_request`,
`review_tool_call`, `register_host_handlers` (`file.edit`, `file.read`), `on_session_start` (status),
`slash_commands` / `execute_slash_command` (`/permissions`), `publish_feature_status`, `confirm_offering`. No
telemetry events.
