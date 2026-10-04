# pa-toolforge

The host half of `rlm.toolforge.publish`: the gated path by which the agent turns Python it wrote into a durable
kernel skill. A fork feature crate (see `docs/fork-feature-crates.md`); built into `prime-agent` through `pa-cli`'s
`toolforge` Cargo feature (on by default), absent from `--no-default-features`.

## Scope

- Serve the kernel host request `toolforge.publish` (`{name, source, doc, exit_test}`), which the runtime's
  `rlm.toolforge.publish` (`prime-agent-runtime/src/rlm/toolforge.py`) sends.
- Validate the name: skill charset and shape, plus collisions with Python builtins, keywords, stdlib modules, the
  kernel's own bindings and the session's loaded Python skills.
- Stage the package (`SKILL.md`, `pyproject.toml`, `src/<import>/__init__.py`, `_exit_test.py`) and a stub whose
  every attribute raises `NotImplementedError`.
- Run the **double-run gate**: the exit test must raise against the stub and run clean against the package. Each run
  is an isolated interpreter (`-I -B`, program on stdin) in the inherited environment with the package root first on
  `PYTHONPATH`, leading its own process group, which is killed as soon as the run ends or times out (30 s).
- Promote an accepted package by rename into `<agentDir>/skills/<name>` under the kernel bootstrap lock, then
  editable-install it into the kernel venv. The runtime binds it in the publishing cell; later sessions discover it as
  an ordinary Python skill.
- Record every attempt (accepted or refused) in the ledger.

## Non-goals

- No kernel or runtime changes: binding the module in `__main__` is the runtime's job.
- No refinement-screen integration (the TS dry-run screen prepended `published_packages` source roots); the Rust
  refinement screen does not consume the ledger yet.

## Public API

- `ToolforgeFeature` (`pa_core::features::SessionFeature`), `ToolforgeOverrides` (paths, interpreter, installer,
  timeout for tests and embedders).
- `register_publish_handler`, `publish`, `PublishRequest`, `PublishOptions`, `PublishResult`, `RejectionStage`,
  `PackageInstaller`, `kernel_venv_installer`.
- Ledger: `ledger_path`, `load_ledger`, `published_packages`, `Ledger`, `LedgerRecord`, `GateRun`, `GatePhase`,
  `PublishStatus`, `PublishedPackage`.
- `validate_name`, `RESERVED_IMPORT_NAMES`, `PUBLISH_REQUEST`, `PUBLISH_EVENT`, `DEFAULT_GATE_TIMEOUT`.

## Seams

- `pa_core::features::SessionFeature::register_host_handlers` — registers `toolforge.publish` per session; reads
  `SessionFeatureContext::{agent_dir, session_id, python_skill_import_names, telemetry}`.
- `pa_core::kernel::bootstrap::install_python_skill_package` / `installed_kernel_python` — the generic
  one-package editable install under the bootstrap lock, and the kernel interpreter on disk.
- `pa_core::platform::process` (process groups) and `pa_core::kernel::orphan_journal` (orphan reaping).

## Files owned

- `<agentDir>/toolforge/ledger.json` — byte-compatible with the TS product (`{schema: 1, records: [...]}`, two-space
  JSON, trailing newline, camelCase keys; at most 500 records).
- `<agentDir>/toolforge/staging/` — per-attempt scratch, removed after each attempt.
- `<agentDir>/skills/<name>/` — the promoted package (generated files byte-identical to the TS product's).

## Telemetry

- Event `toolforge publish` (catalog schema v4): `status` (`published`/`rejected`), `rejection`
  (`name`/`shape`/`negative`/`positive`/`error`, null when published), `installed`, `gate_run_count`, `version`,
  `duration_ms`. Never a name, source, exit test or path.
- Tracing spans `toolforge.publish` and `toolforge.gate` with the TS span attributes.
