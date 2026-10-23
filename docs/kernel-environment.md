# Kernel environment and snapshots

The Python kernel (the `ipython` tool) and every process it starts through `bash()` run with
an environment derived from the Prime Agent process. Two rules decide what reaches them and
what is written to disk.

## `kernel.environment`

Set in the global settings file (`~/.prime/agent/settings.json`):

```json
{ "kernel": { "environment": "scrub-credentials" } }
```

| Value | The kernel and its `bash()` children inherit |
| --- | --- |
| `inherit` (default) | The whole host environment, as before. |
| `scrub-credentials` | The host environment without the model-provider API keys Prime Agent itself reads (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `ANTHROPIC_OAUTH_TOKEN`, `PRIME_API_KEY`, `GEMINI_API_KEY`, `OPENROUTER_API_KEY`, `HF_TOKEN`, `COPILOT_GITHUB_TOKEN`, and the other provider keys). |

Notes:

- The kernel never needs these keys: `rlm.spawn` and the other model calls go through the
  host. Code you run in the kernel that uses a provider SDK directly (or `HF_TOKEN` for the
  Hugging Face libraries) has to bring its own key under `scrub-credentials`.
- `GH_TOKEN` and `GITHUB_TOKEN` are not removed: tools such as `gh` use them, and Prime Agent
  only falls back to them for the GitHub Copilot provider.
- Other credentials in your environment (cloud provider credentials, database URLs, and so
  on) are inherited under both values.
- The setting is read from the global settings only. A project's `.prime/agent/settings.json`
  cannot set or override it.
- The daemon worker's internal identity variables are never inherited, whatever the setting.
- The setting applies when a kernel starts. Restart the session (or the kernel) after you
  change it.

## `kernel.editReadCheck`

The bundled `edit` skill (`edit.run`, `edit.write`, `edit.notebook_edit`) compares each write
with what the model last read in the kernel, as Claude Code's Edit tool does. The kernel
records the mtime and size of every file a cell opens for reading; the kernel's own writes
count as known content.

```json
{ "kernel": { "editReadCheck": "read" } }
```

| Value | A write to an existing file is refused when |
| --- | --- |
| `stale` (default) | the file changed on disk since the model last read it ("File has been modified since read, either by the user or by a linter. Read it again before attempting to write it."). A file the model never read in the kernel is not checked. |
| `read` | as `stale`, and also when the model never read the file in the kernel ("File has not been read yet. Read it first before writing to it."). Reads through `bash()` (`cat`, `rg`) do not count. |
| `off` | never. |

The global scope sets it, and a project `.prime/agent/settings.json` once the workspace is
trusted (an untrusted repo cannot weaken the rule to `off`); it reaches the kernel as
`PRIME_AGENT_EDIT_READ_CHECK` when the kernel starts.

## Kernel snapshots

A persistent session saves the kernel's variables to `kernel-state.dill` in the session's
artifact folder, so a resumed session gets them back. The snapshot never stores:

- variables whose name looks like a credential (`api_key`, `access_token`, `GITHUB_TOKEN`,
  `db_password`, `clientSecret`, `private_key`, `cookies`, ...);
- values that contain a credential-shaped string (for example `sk-...`, `ghp_...`,
  `AKIA...`, `xoxb-...`, a private-key PEM block, or a JWT), or the value of a credential-named
  environment variable that is currently set;
- `os.environ` or a copy of most of it.

Such variables are listed as skipped in `kernel-state.json` (names only) and are not revived on
resume; recreate them in the resumed session if you need them.

A snapshot taken in the background (after a cell, or before a restart such as a plan-mode
toggle) has a time budget. When the budget runs out, the variables not yet saved keep their
value from the previous snapshot, or are not saved when there is none; `kernel-state.json`
lists them under `stale`, and the next restore tells the model which variables are older or
missing. A snapshot that cannot run at all (the kernel is stuck in other work) is recorded in
`kernel-state.incomplete.json`, and the next restore says the revived state predates it.
