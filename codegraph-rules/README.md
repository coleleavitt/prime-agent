# codegraph rules for this repo's hazard classes

YAML bug rules for [`codegraph analyze rules`](https://github.com/Miasin-Labs/codegraph-rs)
(1.3.1+). Every rule here is a tree-sitter query with `where` predicates:
weggli patterns only parse C/C++, so none of them fit Rust or Python.

## Running them

```sh
codegraph init .                        # once per checkout (writes .codegraph/, untracked)
codegraph compiler-sync .               # optional: rust-analyzer-resolved call edges
codegraph analyze rules --check codegraph-rules/          # each rule's bad/good examples; no index needed
codegraph analyze rules -p . codegraph-rules/ --no-saved  # product code
codegraph analyze rules -p . codegraph-rules/ --no-saved --tests  # include test code
codegraph analyze review -p . --rules codegraph-rules/ --rule pa-spawn-without-text-busy-retry --top 5
```

`--check` must stay green: every rule carries `bad` examples it has to match and
`good` examples it must not. `--json` gives the findings as data; `--sarif FILE` writes SARIF.

## The rules

Hit counts are distinct sites on the tree the rules were written against
(`fix-bughunt`, after its fixes). A finding is a lead: each rule's `review:` list says what
to read to confirm or dismiss it.

| Rule | File | Catches | Sites |
| --- | --- | --- | --- |
| `pa-lock-unwrap-outside-tests` | `locks.yaml` | `.lock()/.read()/.write()` + `unwrap`/`expect` outside tests | 758 |
| `pa-std-guard-across-await` | `locks.yaml` | a sync guard bound by `let` in async code, with a later statement that awaits | 0 (22 in tests) |
| `pa-spawn-without-text-busy-retry` | `process.yaml` | `Command::new(<computed path>)` in a function that never calls `*_retrying_text_busy` | 29 |
| `pa-test-env-mutation-without-lock` | `tests-isolation.yaml` | `env::set_var`/`remove_var` in a test that takes no env lock | 105 |
| `pa-test-proc-self-fd-number` | `tests-isolation.yaml` | a test reading `/proc/self/fd/<n>` | 1 |
| `pa-test-global-count-assertion` | `tests-isolation.yaml` | `assert_eq!(live_*_count(), N)` / `registry().len()` in a test | 0 |
| `pa-unvalidated-name-path-join` | `paths.yaml` | `dir.join(name)` / `dir.join(format!("{id}.jsonl"))` with no validator in the function | 41 |
| `pa-float-parse-js-parity` | `floats.yaml` | `parse::<f64>()`, `f64::from_str`, `from_str::<f64>` outside tests | 15 |

### `pa-lock-unwrap-outside-tests`

A panic while any holder has the guard poisons a std lock, and every later
`.lock().unwrap()` then panics too. In the daemon, supervisor and session worker
(long-lived processes; tokio catches the first panic per task) that cascades.
The repo's poison-tolerant forms are the `lock()` helpers and
`.unwrap_or_else(PoisonError::into_inner)`. The hits are the plain Rust idiom
spread through `pa-daemon` (517), `pa-core`, `pa-agent`. None is a bug alone; the
hazard is the mix of conventions. Read it as a migration list (or a reason to
switch to non-poisoning `parking_lot` locks), not as 758 bugs.

### `pa-std-guard-across-await`

`clippy::await_holding_lock` catches std guards at compile time. This rule also
reaches the `lock(&m)` wrappers and needs no build. It skips tokio guards
(`.lock().await`), guards consumed in the `let` (`lock(..).clone()`,
`{ *m.lock().unwrap() }`), statements after a `drop(..)`, and awaits inside a
spawned `async` block. Product code has no hits. The 22 test sites are the
crate-wide test serialization locks (`FAUX_TEST_LOCK`, the kernel test locks),
which are held across the test body on purpose.

### `pa-spawn-without-text-busy-retry`

Executing a file that is open for writing fails with ETXTBSY. In a
multi-threaded process a fork can hold an inherited write handle until its exec,
and a concurrent bootstrap may be rewriting a venv interpreter. Fresh binaries
should be spawned through
`pa_core::platform::process::{spawn_retrying_text_busy, status_retrying_text_busy}`.
Literal programs (`"git"`) are skipped. The hits that matter are the ones whose
path can name a just-written file: kernel and venv interpreters, staged update
binaries, and test shims. Already fixed: the kernel start (`kernel/manager/startup.rs`).
Still open: the toolforge gate and ravo runner spawn the kernel venv python. The
rest spawn the running binary (`current_exe`, already executing), a binary
probed once before (safe after one successful exec), user tools (shell, editor,
browser, clipboard), or PATH tools.

### `pa-test-env-mutation-without-lock`

Tests in one binary share one process environment. Two locks that guard the same
variable do not serialize each other, and neither does a test that takes no lock.
`impl` blocks are skipped (guard types' `apply`/`drop`: the constructor holds the
lock). Confirmed races found with this rule:
`revival_gate`'s two tests remove `PRIME_AGENT_INTERNAL_SESSION_LEASES` while
`lease::lease_conflicts_and_releases` relies on it, and `provider_login`'s tests
remove `PRIME_AGENT_TRACES_API_KEY` while `client_traces` sets it under a private
lock. `config::expands_tilde` sets `HOME` and never restores it. Most of the
remaining hits are single-test integration binaries (no concurrency), the
idempotent `remove_var("TMUX")` in the `pa-tui` headless tests, or variables
nothing else in the binary reads.

### `pa-test-proc-self-fd-number`

The fd table is process-wide: once an fd closes, another test thread can reuse
its number at once. The only hit is `socket.rs`'s `fd_target`, which already
compares the link target (the fix in 95e7d1439), so it is the safe form.

### `pa-test-global-count-assertion`

A test asserting an exact count on a process-wide registry races the other tests
in the binary (the `live_kernels` fix in 4f08bda73). No sites are left. The rule
guards against the pattern coming back.

### `pa-unvalidated-name-path-join`

Names reach paths from the kernel (`HostRequestPayload` data the agent's Python
writes), daemon socket frames, model tool-call arguments and files on disk.
Found with this rule and fixed: `prepare_update_restart`'s `updateId` named a
directory that an abort or expiry deleted with `remove_dir_all`. Validated or
derived elsewhere (false positives): toolforge names (`validate_name`), dream ids
(numeric seeds, enum tasks, allowed arms), worker/session ids the daemon mints,
and names read back from directory listings.

### `pa-float-parse-js-parity`

Rust's float parser is not JavaScript's `Number()`. It rejects `" 1"`, `""` and
`0x1f`, and it accepts `inf`, `infinity` and `nan` in any case. serde_json
without `float_roundtrip` parses best-effort, 1 ULP off for many TS-written
numbers. The workspace now enables `float_roundtrip`. Small deltas remain open at
the sites that document `Number(...)` parity: `supervisor_lost::lost_exit_ms_from`
(`""` reads as 0 in JS), `tailscale::args::js_number` and
`pa-trace::health::js_number_arg` (`"inf"` is NaN in JS).
