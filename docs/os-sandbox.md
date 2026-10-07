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

## MCP servers

`rlm.mcp` starts a stdio MCP server as a kernel child, so the server runs under the same
sandbox: with `network` off it cannot reach the network, and it writes only where the kernel
may. HTTP MCP servers are reached by the kernel itself, so they also need `network: true`.
When stdio servers move to the Prime Agent host, the host must start them under the same
policy (`SessionSandbox::command`), so the move does not loosen the sandbox.

## Not confined

- The Prime Agent daemon, the session worker and the TUI.
- Signals and abstract unix sockets on Linux, and per-path exceptions inside a writable root
  (a writable workspace's `.git` is writable).

The setting applies when a kernel starts. Restart the session (or the kernel) after you change
it.
