# pa-bash

The kernel `bash()` implementation: the shell command checker (the refusal
guards over one command model, and their pipeline) and the job runner the host executes kernel commands
with. The model calls `rlm.bash(...)` in its Python kernel; the kernel is a thin
client of this crate (`prime-agent-runtime/src/rlm/bash.py`), reached through
the host's `bash.*` kernel host requests (pa-core serves them natively, one
`JobTable` per kernel manager) or, for a runtime outside a Prime Agent host,
through the `prime-agent --prime-agent-bash-host` sidecar (`serve_stdio`).

## Scope

- `syntax/`: the one parser. A script becomes a typed tree (`ast`): lists,
  pipelines, compound commands, simple commands whose words keep their
  quoting per part, redirects with here-document bodies. Linear in the text;
  nesting past a bound is kept as unread text, and a line bash would reject
  is not modelled (bash runs nothing of it).
- `model/`: the command model every rule judges. One walk over the tree in
  execution order resolves what the text fixes (variables, positional
  parameters, `cd` targets, functions and aliases at their calls, `hash -p`),
  takes wrappers off (`env`, `sudo`, `timeout`, `nice`, `xargs`, `find
  -exec`, ...) and descends into nested code (`sh -c`, `eval`, `ssh host CMD`,
  here-documents and pipes fed to a shell, script files it can read). Code it
  cannot read becomes an explicit `Opaque` node carrying the visible text it
  comes from.
- `guards/`: the refusal rules, one small module each, every one a `Rule`
  over the model answering with its own message: destructive git discards on a
  dirty tree (`destructive_git`), recursive chmod/chown escaping the workspace
  (`destructive_chmod`), force-pushes to protected targets (`force_push`),
  secret echoes (`secret_echo`), downloads run by a shell (`pipe_to_shell`),
  and sudo/doas (`sudo`). `pipeline` builds the model once and runs them in that
  order; the first refusal wins (`Refusal`: guard, message, one-time
  late-bypass warning).
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
- `service`: the JSON request surface (`handle`, `handle_cancellable`,
  `REQUEST_TYPES`) shared by both transports; `bash.run` checks, spawns and
  follows a command in one request (`run`: the follow window and its
  cancellation, `RunCancel`), and long results can travel in a spill file;
  `sidecar`: the stdio transport (with `{"id", "cancel": true}` frames).
- `sandbox`: `JobSandbox`, the OS sandbox (`pa-os-sandbox`) every process
  the crate starts runs under: the kernel's own prepared restriction (pa-core
  sets it at each kernel start), none, or unavailable (nothing starts). The
  sidecar takes it from the `sandbox` setting (`docs/os-sandbox.md`).
- `shell`: the kernel's shell choice and child environment (non-interactive
  settings, guard bypass scrub, `BASH_ENV`/`ENV`/`BASH_FUNC_*` dropped).

## Non-goals

- No session, kernel-manager or wire knowledge: the caller supplies the
  kernel's cwd and environment (`GuardContext`) with every request.
- The REPL cell lifecycle (one-shot ownership of an awaited command, the
  background completion notice and its withdrawal) and the `bash.command`
  trace span stay in the kernel. Plan mode is a sandbox policy the caller
  sets on the job table (`JobSandbox`), not a per-request concern.

## Public API

- `check(&Script, &Allowances, &GuardContext) -> Result<(), Refusal>`;
  `Script`, `Allowances`, `GuardContext`, `GuardKind`, `Refusal`
- `JobTable` (`new`, `set_sandbox`, `sandbox`, `kill_all`, `activity`,
  `inventory`), `JobSandbox`, `SpawnRequest`,
  `SpawnError`, `ActivityError`
- `handle(&JobTable, &Value) -> Value`, `handle_cancellable(.., &RunCancel)`,
  `RunCancel`, `REQUEST_TYPES`, `serve_stdio(JobSandbox)`
- `child_env`, `resolve_shell`, `ShellError`, `is_truthy_env_value`

## Dependencies

`serde`, `serde_json`, `thiserror`, `regex` (the secret-echo
rule reads grep patterns), `getrandom` (the fence token and
job ids),
`memchr` (the marker and cargo-lock searches over every output read),
`process-wrap` (safe `setsid` in the child and Windows job objects, which std
cannot express without `unsafe`), `rustix` on POSIX (group signals, `poll`,
`FIONREAD`), and `pa-os-sandbox` (the confinement the kernel's commands run
under; a leaf crate, so pa-bash stays below pa-core).

## Design: refuse on evidence

The guards came from six separate Python scanners, each with its own lexer,
each failing closed whenever it could not analyse a construct, whether or not
the command showed its danger. Measured on 199k real commands they refused
3.7% of them, mostly for running a script, a login shell or `ssh host bash -s`.
The redesign keeps the dangers and drops the guessing:

1. **One parse, one model.** `syntax` parses once; `model` resolves what the
   text fixes and descends into nested code. Every rule reads the same model.
2. **Small rules.** A rule matches its danger on resolved argv (a `git push`
   whose refspec names `main` with `-f` in force, a recursive chmod whose
   target resolves outside the workspace) and reads the filesystem or a
   bounded read-only git probe only where the danger depends on it.
3. **Evidence-gated opacity.** Code the model cannot read is an `Opaque` node
   (a missing script, a shell reading a stream, `eval "$x"`, a command word
   only known at run time). A rule refuses one only when the node's own
   visible source carries the rule's evidence: `push` and a force token,
   `chmod`/`chown`/`chgrp` and `-R`, `sudo`/`doas`, `curl`/`wget` feeding code,
   an environment dump or a secret path, `git` and a discard verb. Readable
   code is read: a script file anywhere on disk (or written earlier by the
   same command), whether a shell is given it (`bash x.sh`, `source x.sh`,
   with bash's PATH lookup), runs it by path (`./x.sh` with a shell `#!` or
   none), or sources it at startup (a login profile, an interactive rc file,
   `$BASH_ENV`); a here-document, a `-c` payload, an `xargs` input the text
   fixes.
4. **Word shape.** A word with a literal start keeps it: `offer/$n` is never a
   flag or a `+refspec`. A value only known at run time counts as a force
   only when the text it comes from shows one.
5. **Messages name the evidence**: the payload, script or wrapper a command
   sits in, and the command itself.
6. **Probes run nothing.** A git probe replays only what selects the
   repository (`-C`, `--git-dir`, `GIT_DIR` and friends, non-executing `-c`
   keys) and runs with every program git could start through configuration
   or environment disarmed: fsmonitor, hooks, pagers, editors, ssh and
   credential commands, filter, diff and merge drivers (blanked by name from
   the effective configuration), with `GIT_OPTIONAL_LOCKS=0`.

The OS sandbox (`pa-os-sandbox`) contains what runs; the guards are speed
bumps for the dangers they name, not a sandbox.

Tests: `prime-agent --prime-agent-bash-host` with
`PA_TEST_BASH_HOST_ROOT=<dir>` is a test host that re-executes
itself under the `read-only` OS sandbox with `<dir>` as its only writable
directory and no network (and refuses every command where the sandbox cannot
be enforced). The Python guard suites start their host this way
(`prime-agent-runtime/test/guard_safety.py`), and a case that expects a
refusal is only ever checked, never run.

## Parity

`tests/corpus/guards.jsonl` (+ `messages.json`) holds every input the Python
guard suites fed the guards, judged by the Python guards in a neutral context
(1618 inputs x 6 guards), captured before their deletion.
`guards::corpus_tests` replays it on every test run: each verdict matches the
Python one, or `tests/corpus/deltas.jsonl` records the difference with its
category (no evidence, resolved, unset variable, not run, narrowed policy,
new refusal).

## Parser choice

The model keeps a hand-written parser. Re-evaluated for the redesign:
yash-syntax is GPL-3.0-or-later (not in `deny.toml`'s allowlist);
tree-sitter-bash produces error trees and needs a C build on every target;
brush-parser returns no partial tree for input it rejects. The hand-written
parser drops text on 3 of the 198,784 real commands `bash -n` accepts (all
three a backquote inside double quotes whose body bash parses only at run
time; the parser keeps that body as unread text instead).
