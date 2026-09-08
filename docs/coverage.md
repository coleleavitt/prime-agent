# Coverage

CI collects JavaScript and Python coverage for each test runner and uploads one aggregated set of reports to Codecov. Missing reports and upload errors fail the workflow. Codecov emits non-informational project, patch, and component statuses with absolute coverage floors; repository branch protection must require those Codecov checks because Codecov computes them asynchronously after upload.

## Local commands

Run package coverage from the repository root:

```bash
npm --prefix packages/agent run test:coverage
npm --prefix packages/ai run test:coverage
npm --prefix packages/tui run test:coverage
npm --prefix packages/coding-agent run test:coverage
npm --prefix packages/coding-agent run test:coverage:process
npm --prefix packages/coding-agent run test:coverage:kernel
(cd prime-agent-runtime && uv run coverage run -m unittest discover -s test && uv run coverage xml)
```

The coding-agent default, process-heavy regression, and kernel suites run separately in CI. Each CI matrix job publishes a uniquely named coverage artifact. The coverage job downloads all artifacts before one Codecov upload, so sharded reports cannot overwrite each other.

Codecov enforces a 70% project floor and an 80% patch floor for the whole repository and for the `coding-agent`, `agent-core`, `ai`, `tui`, and `runtime` components. Configuration lives in [`codecov.yml`](../codecov.yml). Configure branch protection to require every Codecov project, patch, and component status; `fail_ci_if_error` covers uploader failures, not those later status results.
