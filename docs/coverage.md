# Coverage

No CI workflow collects or uploads line coverage today. The TS product's coverage lanes (per-package
`test:coverage` scripts, one aggregated Codecov upload) were deleted with the TS packages, and nothing replaced them
for the Rust crates: no workflow under `.github/workflows/` runs a coverage tool or uploads to Codecov.

## What exists

- **Test-selection completeness, not line coverage.** `ci.yml` shards the Rust test binaries with
  `scripts/ci_test_shard.py`; the summary job (`scripts/ci_test_shard_summary.py`) checks that the union of the shard
  manifests equals the full enumeration, so a test target cannot silently drop out of the run. `make shard-gates`
  runs the scripts' own tests.
- **Python runtime coverage, local only.** `prime-agent-runtime/pyproject.toml` carries `coverage[toml]` in the `dev`
  dependency group and the `[tool.coverage.run]` / `[tool.coverage.xml]` config (branch coverage over `rlm`, XML to
  `coverage/coverage.xml`):

  ```bash
  (cd prime-agent-runtime && uv run coverage run -m unittest discover -s test && uv run coverage xml)
  ```

  CI runs the same suite without coverage (`uv run --locked python -m unittest discover -s test` in `ci.yml`).

## Not ported

- Rust line coverage. No `cargo llvm-cov` (or similar) lane is configured.
- The Codecov gate. [`codecov.yml`](../codecov.yml) still holds the TS-era floors (70% project, 80% patch) and its
  components still name `packages/**` paths, which no longer exist; with no upload it has no effect. Re-enabling it
  needs a coverage lane per runtime and components keyed to the crate directories under `crates/`.
