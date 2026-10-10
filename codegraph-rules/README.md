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
| `pa-lock-unwrap-outside-tests` | `locks.yaml` | `.lock()/.read()/.write()` + `unwrap`/`expect` outside tests | 10 (was 758) |
| `pa-std-guard-across-await` | `locks.yaml` | a sync guard bound by `let` in async code, with a later statement that awaits | 0 (22 in tests) |
| `pa-spawn-without-text-busy-retry` | `process.yaml` | `Command::new(<computed path>)` in a function that never calls `*_retrying_text_busy` | 29 |
| `pa-test-env-mutation-without-lock` | `tests-isolation.yaml` | `env::set_var`/`remove_var` in a test that takes no env lock | 105 |
| `pa-test-proc-self-fd-number` | `tests-isolation.yaml` | a test reading `/proc/self/fd/<n>` | 1 |
| `pa-test-git-without-isolation` | `tests-isolation.yaml` | `Command::new("git")` in a function that neither uses the fixture helper nor scrubs `GIT_DIR` | 1 (was 4 in pa-core tests alone) |
| `pa-test-global-count-assertion` | `tests-isolation.yaml` | `assert_eq!(live_*_count(), N)` / `registry().len()` in a test | 0 |
| `pa-test-spawn-without-state-isolation` | `tests-isolation.yaml` | `Command::new(..)` in a function naming `CARGO_BIN_EXE_*` that never uses `TestState` | 115 in 93 files before `fix-test-isolation` |
| `pa-test-engine-on-resolved-agent-dir` | `tests-isolation.yaml` | a test's `agent_dir:` field set from `agent_dir()` / `get_agent_dir()` / `home_dir()` | 0 |
| `pa-unvalidated-name-path-join` | `paths.yaml` | `dir.join(name)` / `dir.join(format!("{id}.jsonl"))` with no validator in the function | 41 |
| `pa-float-parse-js-parity` | `floats.yaml` | `parse::<f64>()`, `f64::from_str`, `from_str::<f64>` outside tests | 15 |

### `pa-lock-unwrap-outside-tests`

A panic while any holder has the guard poisons a std lock, and every later
`.lock().unwrap()` then panics too. In the daemon, supervisor and session worker
(long-lived processes; tokio catches the first panic per task) that cascades.
The convention is `pa_types::sync::{MutexExt::lock_or_recover,
RwLockExt::read_or_recover, RwLockExt::write_or_recover}`, which recover the
guard through `PoisonError::into_inner`; the leaf crates with no pa-types
dependency (pa-agent, pa-telemetry, pa-mermaid) spell it
`.unwrap_or_else(PoisonError::into_inner)`. `fix-lockpoison` migrated all 748
product sites (pa-daemon 508, pa-core 95, pa-agent 62, pa-tui 31, pa-ai 26,
pa-cli 12, pa-models 9, pa-telemetry 3, pa-mermaid 2); no site was kept as an
unrecoverable invariant. A new hit is a regression: convert it, or, if a
panicking holder can leave the value half-updated and unsafe to read, keep the
`expect` with a comment saying so and list it here.

The 10 remaining hits are not product code: helpers in test modules gated at
their `mod` declaration (`worker/turn_stream_tests/*`, `cloud_guest/tests_support.rs`,
which `is-test` cannot see) and `docs/evidence/stall-2026-09-07/historical-eventbus-deadlock.rs`
(a historical excerpt, not compiled).

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

### `pa-test-git-without-isolation`

Git exports `GIT_DIR`, `GIT_WORK_TREE` and their siblings to hooks and `git rebase --exec`
commands. Tests gated that way inherited the outer repository's selection, and their fixture
git (`current_dir(tempdir)` changes nothing) re-initialized the real `.git` as bare, wrote a
`[user]` section into its config, and created a branch and commits in it. Test fixtures build
repositories through `pa_core::git_env::{fixture_git, run_fixture_git}` (pa-core unit tests:
`test_support::run_git`; pa-bash: `test_support::{git, run_git}`); product code that acts on a
directory it owns scrubs with `pa_core::git_env::scrub_repository_selection`. The rule does not
require `is-test` (fixture helpers are plain functions), so run it with `--tests`.

The one remaining hit honours the selection on purpose: `session/manager/git.rs`
`capture_git_context` describes the repository the user's own `git` would use (TS
`captureGitContext` parity; a dotfiles setup exports `GIT_DIR`/`GIT_WORK_TREE`). The same holds
for the bash tool and the pa-bash guard probes, which run in the guarded command's environment
(they spawn through a shell, so the rule does not see them). The regression tests
`pa_core::git_env::tests::git_tests_leave_an_inherited_repository_untouched` and
`pa_bash::test_support::tests::guard_tests_leave_an_inherited_repository_untouched` re-run the
git-running tests under an exported `GIT_DIR` and require the sentinel repository byte-identical.

### `pa-test-global-count-assertion`

A test asserting an exact count on a process-wide registry races the other tests
in the binary (the `live_kernels` fix in 4f08bda73). No sites are left. The rule
guards against the pattern coming back.

### `pa-test-spawn-without-state-isolation`

A spawned workspace binary resolves its agent dir, `auth.json`, the shared
Anthropic account store (`~/.anthropic-accounts`), Claude Code's credentials
and the kernel venv from the environment the test inherited. A `cargo test`
started from a prime-agent session inherits the session's
`PRIME_AGENT_CODING_AGENT_DIR` (the kernel exports it): on 2026-10-08, 32
spawned binaries that set `HOME` but not the agent dir traced into the real
`~/.prime/agent/logs/agent.jsonl`, read the real `auth.json` and flushed the
real global harness ledger. `pa_types::platform::test_isolation::TestState`
points every one of those paths under the test's temp dir and scrubs the
inherited redirects (`PRIME_AGENT_*`, `PI_*`, `RLM_*`, `OPENCODE_*`,
`ANTHROPIC_ACCOUNTS_*`, `CLAUDE_CONFIG_DIR`); apply it right after
`Command::new`, before the test's own `.env(..)`. The binaries' startup guard
(`test_isolation::refuse_real_state`) and the agent-dir resolvers refuse a
real-state path in any test process (`PA_TEST_ISOLATED`, `CARGO_BIN_EXE_*` in
the environment, or a `deps/<name>-<hash>` harness), judged against the passwd
home rather than `$HOME`; the rule keeps new spawn sites isolated rather than
merely refused. Functions naming `PROTECTED_HOME_ENV` are the guard's own tests,
which leak on purpose against a sentinel home.

### `pa-test-engine-on-resolved-agent-dir`

Every in-process engine in the tests takes an explicit temp agent dir. A test
that passes the environment-resolved one instead (`agent_dir()`, `home_dir()`)
works on the real `~/.prime/agent` under the real `HOME`; the resolvers panic
there in test processes. No sites; the rule guards the pattern.

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
