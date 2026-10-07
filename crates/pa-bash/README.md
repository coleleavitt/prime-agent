# pa-bash

The kernel `bash()` implementation: the shell command checker (the six refusal
guards and their pipeline) and the job runner the host executes kernel commands
with. The model calls `rlm.bash(...)` in its Python kernel; the kernel is a thin
client of this crate (`prime-agent-runtime/src/rlm/bash.py`), reached through
the host's `bash.*` kernel host requests (pa-core serves them natively, one
`JobTable` per kernel manager) or, for a runtime outside a Prime Agent host,
through the `prime-agent --prime-agent-bash-host` sidecar (`serve_stdio`).

## Scope

- `guards/`: the refusal guards, one module directory each, every one
  answering with its exact refusal message: destructive git discards on a
  dirty tree (`destructive_git`), recursive chmod/chown escaping the workspace
  (`destructive_chmod`), force-pushes to protected targets (`force_push`),
  secret echoes (`secret_echo`), downloads piped into a shell
  (`pipe_to_shell`), sudo/doas (`sudo`). `pipeline` runs them in that order;
  the first refusal wins (`Refusal`: guard, message, one-time late-bypass
  warning).
- `probe`: the guards' read-only shell probes (`git status`, the upstream of
  the current branch) in the kernel's shell and environment, own process
  group, bounded wait and output read.
- `runner/`: spawning a checked command (`job`), the completion fence and the
  status channel (`fence`), the head+tail output buffer (`buffer`), progress
  events (`command_progress`, `cargo_lock_wait`, `command_no_output`), the
  orphan-process journal (`journal`), and the activity view (list/tail/kill
  with the kernel's 16 KiB frame caps).
- `platform/`: containment. POSIX: a new session per command (process group =
  pid, no controlling terminal), the status socket as stdin, group signals,
  `FIONREAD` for the drain. Windows: a kill-on-close job object entered while
  the child is suspended; no status channel (the result is final at exit).
- `service`: the JSON request surface (`handle`, `REQUEST_TYPES`) shared by
  both transports; `sidecar`: the stdio transport.
- `shell`: the kernel's shell choice and child environment (non-interactive
  settings, guard bypass scrub, `BASH_ENV`/`ENV`/`BASH_FUNC_*` dropped).

## Non-goals

- No session, kernel-manager or wire knowledge: the caller supplies the
  kernel's cwd and environment (`GuardContext`) with every request.
- The REPL cell lifecycle (one-shot ownership of an awaited command, the
  background completion notice and its withdrawal), the `bash.command` trace
  span and plan mode's classification stay in the kernel.

## Public API

- `check(&Script, &Allowances, &GuardContext) -> Result<(), Refusal>`;
  `Script`, `Allowances`, `GuardContext`, `GuardKind`, `Refusal`
- `JobTable` (`new`, `kill_all`, `activity`, `inventory`), `SpawnRequest`,
  `SpawnError`, `ActivityError`
- `handle(&JobTable, &Value) -> Value`, `REQUEST_TYPES`, `serve_stdio()`
- `child_env`, `resolve_shell`, `ShellError`, `is_truthy_env_value`

## Dependencies

`serde`, `serde_json`, `thiserror`, `getrandom` (the fence token and job ids),
`process-wrap` (safe `setsid` in the child and Windows job objects, which std
cannot express without `unsafe`), `rustix` on POSIX (group signals, `poll`,
`FIONREAD`). No workspace crate.

## Parity

The Python implementation (`prime-agent-runtime/src/rlm/bash.py` before the
port) is the oracle:

- `tests/corpus/guards.jsonl` + `messages.json`: every input the Python guard
  suites fed the guards, judged by all six Python guards in a neutral context
  (`tests/corpus/capture.py`). `guards::corpus_tests` replays it.
- `tests/corpus/differential.py` with `examples/guard_oracle.rs`: runs the
  Python suites with every guard call also judged by the Rust guards in the
  call's live context (files, repositories, environment). (It needs the
  Python guards, so it ran before the switch-over; after it the Python guard
  suites themselves run against these guards through `bash.check`.)

## Parser choice

The guards keep a hand-written lexer. Evaluated on the 1618 corpus inputs:
yash-syntax is GPL-3.0-or-later (not in `deny.toml`'s allowlist);
tree-sitter-bash 0.25.1 produces ERROR trees for 43 inputs, 4 of them inputs
every Python guard allows, and needs a C build on every target; brush-parser
0.4.0 parses 1606 but returns no partial tree for the 12 it rejects (10 of
them inputs the guards refuse for a guard-specific reason, e.g. `echo $(sudo
id`) and brings 65 crates. None reproduces the guards' fail-closed reading of
malformed input.
