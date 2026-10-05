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
