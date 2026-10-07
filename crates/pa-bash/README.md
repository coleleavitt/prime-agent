# pa-bash

The kernel `bash()` implementation: the shell command checker (the six refusal
guards and their pipeline) and the job runner the host executes kernel commands
with. The model calls `rlm.bash(...)` in its Python kernel; the kernel is a thin
client of this crate.

## Scope

- The refusal guards, one module each, every one answering with a typed
  verdict: destructive git discards on a dirty tree (`destructive_git`),
  recursive chmod/chown escaping the workspace (`destructive_chmod`),
  force-pushes to protected targets (`force_push`), secret echoes
  (`secret_echo`), downloads piped into a shell (`pipe_to_shell`), sudo/doas
  (`sudo`). The pipeline runs them in that order; the first refusal wins.
- The kernel's command environment (`child_env`) and shell choice
  (`resolve_shell`), shared by the guards' read-only probes and the runner.

## Non-goals

- No session, kernel-manager or wire knowledge: the host (pa-core) serves the
  kernel's requests and owns the protocol; this crate takes a `GuardContext`
  (the kernel's cwd and environment) and text.
- Plan mode stays with the kernel's `rlm.plan_guard` (audit hooks and the
  read-only classification of the Python process itself).

## Public API

- `check(&Script, &Allowances, &GuardContext) -> Result<(), Refusal>`
- `Script` (command, script, trusted prefix), `Allowances` (per-call
  bypasses), `GuardContext` (cwd, kernel env, launch-time bypasses,
  traceparent)
- `GuardKind`, `Refusal` (guard, message, one-time late-bypass warning)
- `child_env`, `resolve_shell`, `ShellError`, `is_truthy_env_value`

## Parity

The Python implementation (`prime-agent-runtime/src/rlm/bash.py` before the
port) is the oracle:

- `tests/corpus/guards.jsonl` + `messages.json`: every input the Python guard
  suites fed the guards, judged by all six Python guards in a neutral context
  (`tests/corpus/capture.py`). `guards::corpus_tests` replays it.
- `tests/corpus/differential.py` with `examples/guard_oracle.rs`: runs the
  Python suites with every guard call also judged by the Rust guards in the
  call's live context (files, repositories, environment).
