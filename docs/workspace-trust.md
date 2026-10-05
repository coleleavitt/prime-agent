# Workspace trust

Opening a directory with Prime Agent must not run code the directory carries.
A cloned repository can commit project configuration under `.prime/agent/`;
the parts of it that can run commands or change the agent's instructions load
only after you trust the workspace.

## What is gated

- `.prime/agent/settings.json` keys outside the safe list below - among them
  `shellPath`, `shellCommandPrefix`, `npmCommand`, `mcpServers`,
  `mcpCatalogSources`, `packages`, `skills`, `prompts`, `themes`,
  `sessionDir`, `autonomous`, `factory`, `telemetry`, and any key Prime Agent
  does not know.
- `.prime/agent/SYSTEM.md` and `.prime/agent/APPEND_SYSTEM.md`.
- `.prime/agent/prompts/` (prompt templates).

These project settings keys still apply in an untrusted workspace, because
they only change presentation or turn behaviour within the providers and
models you already configured: `theme`, `defaultProvider`, `defaultModel`,
`defaultThinkingLevel`, `enabledModels`, `thinkingBudgets`, `steeringMode`,
`followUpMode`, `transport`, `compaction`, `branchSummary`, `retry`,
`terminal`, `images`, `treeFilterMode`, `chatDetail`, `editorPaddingX`,
`autocompleteMaxVisible`, `showHardwareCursor`, `markdown`, `warnings`,
`quietStartup`, `requestTiming`, `enableSkillCommands`.

Not gated: `AGENTS.md` / `CLAUDE.md` project context (the repository's own
instructions to any coding agent), project themes, and everything under your
own `~/.prime/agent/`. Running Prime Agent from your home directory, where the
project config dir is `~/.prime/agent` itself, never asks.

## How you are asked

- Interactive: the first launch in a workspace with gated configuration asks
  once, before the session opens, listing what would load. `y` trusts it;
  any other answer is remembered as "not trusted" and the session runs
  without it.
- Print, JSON, RPC and ACP modes never ask. They print one line to stderr
  naming what was skipped and how to load it, then run without it.
- `--trust-workspace` trusts the current directory for this and later runs.
- Sessions in the daemon read the recorded decision; the daemon never asks.

The decision is stored in `~/.prime/agent/trusted-workspaces.json`, keyed by
the workspace's canonical path and pinned to a SHA-256 of the gated content.
Changing any gated file (or a gated settings key) asks again; editing a safe
key such as `theme` does not. A decision applies to sessions started after it;
running sessions keep what they loaded.

## Commands

```sh
prime-agent trust [path]     # trust a workspace (default: the current directory)
prime-agent trust --list     # list recorded decisions
prime-agent untrust [path]   # record "not trusted"; it is not asked again until the content changes
```
