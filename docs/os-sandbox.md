# OS sandbox

Prime Agent can run the Python kernel (the `ipython` tool) under an operating-system sandbox.
Everything the kernel starts inherits it: `bash()`, `subprocess`, any program a cell runs. The
`!` command lane in the chat runs under the same sandbox. It is off by default; with it off
nothing changes.

## Setting

In `~/.prime/agent/settings.json`:

```json
{
  "sandbox": {
    "mode": "workspace-write",
    "network": false,
    "writableRoots": ["~/.cache/uv", "../shared-build"]
  }
}
```

| Key | Values | Default |
| --- | --- | --- |
| `mode` | `off`, `read-only`, `workspace-write` | `off` |
| `network` | `true` lets confined processes open network sockets | `false` |
| `writableRoots` | extra writable directories under `workspace-write` (`~/` expands, relative paths resolve against the working directory) | none |

What each mode allows a confined process to do:

| | `read-only` | `workspace-write` |
| --- | --- | --- |
| read and run anything | yes | yes |
| write the working directory | no | yes |
| write `/tmp` and `writableRoots` | no | yes |
| write `$TMPDIR` and the session's own state (snapshot, harness state) | yes | yes |
| write `/dev/null`, the terminal, `/dev/shm` | yes | yes |

A write the sandbox refuses fails with a permission error (`EACCES`, "Permission denied").
With `network` off, every socket outside the unix domain is refused (`EPERM`): TCP and UDP,
loopback included. Pipes and unix sockets keep working.

An unrecognized `mode` value fails closed to `read-only`.

### Project settings

A project's `.prime/agent/settings.json` may only tighten the sandbox: turn it on, choose a
stricter mode (`read-only` over `workspace-write`), or set `network: false`. It cannot turn the
sandbox off, enable network, or add `writableRoots`. Because it can only tighten, the `sandbox`
key applies in untrusted workspaces too (see [workspace trust](workspace-trust.md)).

### `--sandbox <mode>`

`prime-agent --sandbox workspace-write` (or `read-only`, or `off`) replaces the configured mode
for one run; `network` and `writableRoots` still come from the settings. `--sandbox off` runs
without the sandbox even when the settings enable it. Subagents spawned by such a run are
created under the same mode.

## What the model and you see

With the sandbox on, the system prompt gets one line naming the mode, what it refuses, and the
enforcing mechanism, so the model expects writes outside the workspace to fail. The TUI tray
shows `sandbox <mode>` (`+net` when network is on). With the sandbox off the prompt and the
tray are unchanged.

## Platforms

- **Linux**: Landlock (Linux 5.13+, the `landlock` LSM enabled; check
  `/sys/kernel/security/lsm`) confines the filesystem. With network off, Landlock's TCP rules
  (Linux 6.7+) and a seccomp filter refuse network sockets. On an older kernel the sandbox
  still runs but some protections are missing; the prompt line and the tray say
  `(degraded)` and name what is not enforced (truncation below Landlock ABI 3, device ioctls
  below ABI 5). A kernel without Landlock cannot be sandboxed.
- **macOS**: the kernel runs under `/usr/bin/sandbox-exec` with a generated Seatbelt profile.
- **Windows**: not supported.

When the sandbox is on but cannot be enforced on this machine, the tray shows
`(unavailable)`, the prompt line says so, and the Python kernel refuses to start: nothing runs
unconfined while the setting asks for confinement. Use `--sandbox off` or set `mode` to `off`.

## `bash()` commands

The host runs the kernel's `bash()` commands on its behalf (they are not children of the kernel
process), so it starts each one under the kernel's own prepared sandbox: the same mode, network
rule and writable roots, with the kernel's working directory as the workspace. The guards'
read-only probes (`git status`, the upstream of the current branch) run under it too. A command
the sandbox refuses fails exactly as it did when it inherited the kernel's confinement.

Outside a Prime Agent session, `rlm.bash` runs its commands through the
`prime-agent --prime-agent-bash-host` sidecar. The sidecar resolves the `sandbox` setting for its
working directory the way a session there would (the global choice, a project file only
tightening it) and confines every command and probe to it, with that directory as the workspace
and the temp directory as scratch; `--sandbox` is a session flag and does not apply. A sidecar
whose setting asks for a sandbox this machine cannot enforce refuses every command. Started from
inside a confined process (a script a sandboxed kernel runs), the sidecar also inherits that
process's confinement.

## MCP servers

The Prime Agent host starts a stdio MCP server on the kernel's behalf (`rlm.mcp`). It runs
under the session's sandbox, like the kernel: it writes only its working directory (under
`workspace-write`) and its temp directory, and with `network` off it cannot open network
sockets, so a server that needs the network needs `network: true`. The reasoning: the model
drives these servers, so leaving them unconfined would be a way around the sandbox. (The Codex
CLI runs MCP servers unconfined; Prime Agent chooses not to.) HTTP MCP servers are reached
by the host and are not affected.

## Plan mode

Plan mode (`/plan`, `--plan`, the plan key) runs on this sandbox. While it is on, the kernel
runs under the stricter of the configured mode and `read-only`, keeping the configured
`network` rule; with the sandbox off, plan mode uses `read-only` with network allowed (plan
mode has never blocked the network). A cell, `bash()`, `subprocess` and code calling the C
library directly through `ctypes` all fail with `EACCES` outside `$TMPDIR`, the user cache
directory and the session's own state.

- **Toggling restarts the kernel.** A running process cannot loosen its Landlock domain, so
  switching plan mode on or off stops the kernel with a final namespace snapshot and starts
  its replacement under the new policy, which restores the namespace (the session gets the
  usual restore notice; values the snapshot cannot hold, such as open files or sockets, do
  not survive). A session without an artifact directory keeps no snapshot and starts with an
  empty namespace. A toggle takes about 200 ms here (stop, spawn, restore, runtime
  bootstrap); a toggle that does not change the policy (the sandbox is already `read-only`)
  keeps the kernel.
- **A busy kernel refuses the toggle.** While a cell or a background `bash()` command runs,
  restarting would abort it, so `/plan` fails with a message naming what is running and the
  mode does not change. In the TUI the plan key queues `/plan` behind the running turn, so
  only background commands get in the way.
- **New spawns follow the policy.** `bash()` jobs and their guard probes run under the
  kernel's sandbox, so they follow the restart. A stdio MCP server started under the other
  policy is restarted on its next use.
- **The user cache directory stays writable.** Plan mode adds `XDG_CACHE_HOME` (else
  `~/.cache`, or `~/Library/Caches` on macOS) to `read-only`'s writable scratch, so a
  dry-run that fills a tool cache (`uv`, `pip`) works, as plan mode's messages promise
  ("write only under temp/cache directories"). A configured `read-only` sandbox is never
  loosened: under it plan mode changes nothing, and the cache directory stays read-only.
- **A workspace inside a writable root stays writable.** Landlock cannot deny a path beneath
  a directory it allows, so a working directory inside `$TMPDIR` (or `/tmp` when `TMPDIR` is
  unset) is not protected by plan mode.

The host still refuses its own `edit`, `write` and `bash` tools in plan mode, whatever the
sandbox does.

### Without an OS sandbox

Where the sandbox cannot be enforced (Linux without Landlock, Windows, macOS without
`/usr/bin/sandbox-exec`) and none is configured, plan mode falls back to the in-kernel guard
(`rlm.plan_guard`): a Python audit hook refuses writes outside the temp and cache directories
and refuses every process spawn with `PlanModeError`, and the host refuses every `bash()`
job. Without a sandbox nothing can run a command read-only, so no command runs. The hook
does not see code that calls the C library directly (`ctypes`). Turning plan mode on says so
in a warning row. (A configured sandbox the machine cannot enforce keeps the kernel from
starting at all, as above.)

## Computer use

The bundled `computer-use` skill is not governed by the OS sandbox. Its kernel package is a thin
client; observation, input, capture and the clipboard run in the Prime Agent host
(`pa-computer-use`, through the `computer_use.*` host requests), outside the kernel's sandbox.
What limits it is its own policy: the user-edited allowlist in
`~/.prime/agent/settings/computer-use.toml`, the system deny-list, the locked-screen check,
the macOS TCC grants, and the secure-field refusals (see the skill's `references/safety.md`).
The sandbox never blocked desktop control anyway: the accessibility bus (D-Bus) and the
Wayland socket are unix sockets, which a confined process may use.

Screenshots are written by the host into `~/.prime/agent/tmp/computer-use/` (mode 0700, each
PNG 0600) and read back by the kernel to attach them. The sandbox confines writes, not reads,
so a sandboxed kernel can still read them, and it cannot write into that directory.

## Not confined

- The Prime Agent daemon, the session worker and the TUI.
- Computer use, which runs in the host (see above).
- Signals and abstract unix sockets on Linux, and per-path exceptions inside a writable root
  (a writable workspace's `.git` is writable).

The setting applies when a kernel starts. Restart the session (or the kernel) after you change
it.
