# pa-os-sandbox

OS-level confinement for the processes Prime Agent runs on the model's
behalf. `pa-core` applies it to the Python kernel (so `bash()`,
`subprocess` and everything else a cell spawns inherit it) and to the
`!` user-bash lane. Off by default; see the `sandbox` setting in
`pa-core`'s settings docs.

## Scope

- The policy vocabulary: `SandboxMode` (`off` | `read-only` |
  `workspace-write`, the setting and `--sandbox` wire names),
  `Confinement`, `NetworkAccess`, `SandboxPolicy` (level, network toggle,
  extra writable roots) and `SandboxPaths` (the launch's workspace and the
  scratch directories the confined process needs in every mode).
- `assess(policy)`: what the policy gets on this machine (the mechanism
  and every protection the kernel cannot enforce), no side effects.
- `prepare(policy, paths)` -> `PreparedSandbox::command(program)`: a
  `std::process::Command` whose children are confined.

| platform | mechanism |
| --- | --- |
| Linux | Landlock (ABI 1+) for the filesystem; with network off, Landlock TCP rules (ABI 4+) and a seccomp filter refusing `socket(2)` outside `AF_UNIX` and the `io_uring` syscalls. Applied between fork and exec. |
| macOS | `/usr/bin/sandbox-exec -p <profile> -D WRITABLE_ROOT_<n>=<path> -- <program>` (the Codex CLI's approach). |
| Windows, other | `SandboxError::Unsupported`. Nothing pretends to confine. |

What a confined process can do:

- read and execute everything;
- write beneath the writable roots: the scratch directories always, plus
  the workspace and the configured extra roots under `workspace-write`;
- write the terminal and sink devices (`/dev/null`, `/dev/zero`,
  `/dev/full`, `/dev/tty`, `/dev/ptmx`, `/dev/pts`, `/dev/shm` on Linux);
- with network off, use pipes and unix sockets only.

Degraded kernels: on Linux, a Landlock ABI below 3 cannot confine
truncation and below 5 cannot confine device ioctls; `Assessment::gaps`
names each. A kernel without Landlock is `Unsupported`, never a silent
pass. With network off, an architecture without a seccomp target leaves
only Landlock's TCP rules (ABI 4+), also reported as a gap.

## Non-goals

- Deciding *when* to sandbox, or reading settings: `pa-core` resolves the
  policy and owns the call sites.
- Confining the daemon, the session worker or the TUI. (Stdio MCP servers
  are kernel children today and inherit the kernel's restriction; a
  host-side spawn must use `pa-core`'s `SessionSandbox::command`.)
- Per-path deny rules beneath a writable root (e.g. a read-only `.git`),
  abstract unix sockets, signal scoping, and the x32 syscall ABI (the
  seccomp filter matches the native `x86_64` numbers).

## `unsafe`

The crate denies `unsafe_code`; one module, `linux::syscalls`, opts back
in for the Landlock ABI probe and the `pre_exec` hook. The landlock
crate's safe `restrict_self` confines the calling process and consumes
the ruleset, and `CommandExt` has no safe confinement hook, so the hook
is `pre_exec`. Everything that allocates (the ruleset descriptor, the BPF
instructions) is built before the fork; the child runs three syscalls
(`prctl(PR_SET_NO_NEW_PRIVS)`, `landlock_restrict_self`, `seccomp`) on that
data, each under a `// SAFETY:` note.

## Seams

None: a leaf crate with no workspace dependencies. `pa-core` depends on
it; nothing else does.

## Files and telemetry

Writes no files and emits no telemetry (`pa-core` reports the mode on
`agent started`).

## Tests

- Unit: wire names, project tightening, writable-root resolution, the
  Linux assessment per ABI, the seccomp program, and the Seatbelt profile
  and launcher arguments (on every platform).
- `tests/linux_enforcement.rs`: real enforcement on the running kernel
  (writes outside the workspace fail with EACCES, read-only refuses
  workspace writes, network off refuses TCP and UDP to a local listener,
  grandchildren inherit the restriction). Skips with a message where
  Landlock is unavailable.
