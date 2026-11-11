# Coverage

There is no CI, and nothing collects line coverage for the Rust crates.

## What exists

- **Python runtime coverage, local only.** `prime-agent-runtime/pyproject.toml` carries `coverage[toml]` in the `dev`
  dependency group and the `[tool.coverage.run]` / `[tool.coverage.xml]` config (branch coverage over `rlm`, XML to
  `coverage/coverage.xml`):

  ```bash
  (cd prime-agent-runtime && uv run coverage run -m unittest discover -s test && uv run coverage xml)
  ```

## Not ported

- Rust line coverage. No `cargo llvm-cov` (or similar) lane is configured.
