#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "$0")/.." && pwd)
RYE_ROOT=${RYE_ROOT:-/home/cole/RustProjects/active/rye}
RUN_FULL=${RUN_FULL:-1}
cd "$ROOT"

section() { printf '\n## %s\n' "$1"; }
require_commit() { git -C "$1" cat-file -e "$2^{commit}"; printf 'PASS commit %s\n' "$2"; }
require_file() { test -f "$1"; printf 'PASS file %s\n' "$1"; }

section "Prime commits"
for commit in 10a5f2aaf ccfcb1448 5af42a109 bdd1c735b 461835b86 832f6550f 0b8b9275a; do
  require_commit "$ROOT" "$commit"
done

section "Rye commits"
for commit in 9b8be86 42f9417 e18bc47; do
  require_commit "$RYE_ROOT" "$commit"
done

section "Implementation files"
for file in \
  prime-agent-runtime/src/rlm/trace.py \
  prime-agent-runtime/src/rlm/bash.py \
  packages/coding-agent/src/cli/health-command.ts \
  packages/coding-agent/src/core/kernel/bootstrap.ts \
  packages/coding-agent/src/core/orphan-process-journal.ts \
  packages/coding-agent/src/core/process-crash.ts \
  packages/coding-agent/src/modes/daemon/daemon-mode.ts \
  .github/workflows/ci.yml codecov.yml docs/coverage.md \
  docs/stall-hardening-verification.json; do
  require_file "$ROOT/$file"
done
for file in \
  crates/rye-agent-core/src/harness/events.rs \
  crates/rye-coding-agent/src/process.rs \
  .config/nextest.toml .github/workflows/ci.yml .github/workflows/coverage.yml codecov.yml; do
  require_file "$RYE_ROOT/$file"
done

section "Tracked worktree state"
test -z "$(git diff --name-only)"
test -z "$(git diff --cached --name-only)"
test -z "$(git -C "$RYE_ROOT" diff --name-only)"
test -z "$(git -C "$RYE_ROOT" diff --cached --name-only)"
echo "PASS no modified or staged tracked files"

section "Workflow syntax and immutable action refs"
node --input-type=module <<'NODE'
import fs from "node:fs";
import YAML from "yaml";
const rye = process.env.RYE_ROOT ?? "/home/cole/RustProjects/active/rye";
const files = [
  ".github/workflows/ci.yml",
  "codecov.yml",
  `${rye}/.github/workflows/ci.yml`,
  `${rye}/.github/workflows/coverage.yml`,
  `${rye}/codecov.yml`,
];
for (const file of files) YAML.parse(fs.readFileSync(file, "utf8"));
for (const file of files.filter((file) => file.includes("workflows"))) {
  const text = fs.readFileSync(file, "utf8");
  for (const match of text.matchAll(/uses:\s*[^\s@]+@([^\s#]+)/g)) {
    if (!/^[0-9a-f]{40}$/.test(match[1])) throw new Error(`${file}: mutable action ref ${match[1]}`);
  }
}
console.log("PASS workflow YAML and immutable action refs");
NODE

section "Rye clean-checkout integrity"
if git -C "$RYE_ROOT" ls-tree -r --name-only HEAD | grep -qx Cargo.toml; then
  echo "PASS Rye committed baseline includes Cargo.toml"
else
  echo "BLOCKED Rye clean checkout: pre-existing Cargo.toml and repository baseline remain untracked"
  echo "This is an ownership blocker, not a passing clean-checkout result."
fi

if [[ "$RUN_FULL" != 1 ]]; then
  echo "SKIP full execution because RUN_FULL=$RUN_FULL"
  exit 0
fi

section "Prime required check"
npm run check
section "Prime focused hardening"
(cd packages/coding-agent && npx vitest --run \
  test/health-command.test.ts test/daemon-mode.test.ts \
  test/daemon-supervisor-eviction.test.ts test/daemon-supervisor-monitor.test.ts \
  test/kernel-bootstrap.test.ts test/orphan-process-journal.test.ts \
  test/repl-kernel-trace.test.ts test/repl-kernel-parent-watchdog.test.ts \
  test/process-crash.test.ts test/local-log.test.ts \
  test/otlp-export.test.ts test/otlp-export-process.test.ts)
section "Prime Python runtime"
env -u TRACEPARENT -u PRIME_AGENT_SESSION_ID -u PRIME_AGENT_DAEMON_SOCKET \
  -u PRIME_AGENT_DAEMON_TOKEN -u PRIME_AGENT_DAEMON_SOCKET_OWNER_PID \
  -u PRIME_AGENT_DAEMON_SOCKET_OWNER_START_ID -u PRIME_AGENT_DAEMON_CLIENT_PID \
  -u PRIME_AGENT_DAEMON_CLIENT_START_ID \
  bash -c 'cd prime-agent-runtime && uv run python -m unittest discover -s test'
section "Prime coverage lanes"
npm --prefix packages/agent run test:coverage
env -u OPENAI_API_KEY -u ANTHROPIC_API_KEY -u GOOGLE_API_KEY \
  -u GEMINI_API_KEY -u GROQ_API_KEY -u CEREBRAS_API_KEY \
  -u XAI_API_KEY -u MISTRAL_API_KEY -u OPENROUTER_API_KEY \
  -u AWS_ACCESS_KEY_ID -u AWS_SECRET_ACCESS_KEY -u AWS_SESSION_TOKEN \
  npm --prefix packages/ai run test:coverage
npm --prefix packages/tui run test:coverage
env -u PRIME_AGENT_DAEMON_SOCKET -u PRIME_AGENT_DAEMON_TOKEN \
  -u PRIME_AGENT_DAEMON_SOCKET_OWNER_PID -u PRIME_AGENT_DAEMON_SOCKET_OWNER_START_ID \
  -u PRIME_AGENT_DAEMON_CLIENT_PID -u PRIME_AGENT_DAEMON_CLIENT_START_ID \
  npm --prefix packages/coding-agent run test:coverage
env -u PRIME_AGENT_DAEMON_SOCKET -u PRIME_AGENT_DAEMON_TOKEN \
  -u PRIME_AGENT_DAEMON_SOCKET_OWNER_PID -u PRIME_AGENT_DAEMON_SOCKET_OWNER_START_ID \
  -u PRIME_AGENT_DAEMON_CLIENT_PID -u PRIME_AGENT_DAEMON_CLIENT_START_ID \
  npm --prefix packages/coding-agent run test:coverage:process
env -u PRIME_AGENT_DAEMON_SOCKET -u PRIME_AGENT_DAEMON_TOKEN \
  -u PRIME_AGENT_DAEMON_SOCKET_OWNER_PID -u PRIME_AGENT_DAEMON_SOCKET_OWNER_START_ID \
  -u PRIME_AGENT_DAEMON_CLIENT_PID -u PRIME_AGENT_DAEMON_CLIENT_START_ID \
  npm --prefix packages/coding-agent run test:coverage:kernel
(
  cd prime-agent-runtime
  uv run coverage erase
  env -u TRACEPARENT -u PRIME_AGENT_SESSION_ID -u PRIME_AGENT_DAEMON_SOCKET \
    -u PRIME_AGENT_DAEMON_TOKEN -u PRIME_AGENT_DAEMON_SOCKET_OWNER_PID \
    -u PRIME_AGENT_DAEMON_SOCKET_OWNER_START_ID -u PRIME_AGENT_DAEMON_CLIENT_PID \
    -u PRIME_AGENT_DAEMON_CLIENT_START_ID \
    uv run coverage run -m unittest discover -s test
  uv run coverage xml
  test -s coverage/coverage.xml
)

section "Rye full gate"
(
  cd "$RYE_ROOT"
  cargo fmt --all --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo nextest run --workspace --all-targets --all-features --profile ci
  cargo test --workspace --all-features --doc
  cargo doc --workspace --no-deps
  cargo llvm-cov --workspace --all-features --summary-only
)
section "Historical/control deadlock reproduction"
docs/evidence/stall-2026-09-07/reproduce.sh
section "Result"
echo "PASS all executable owned-work gates"
echo "BLOCKED only: Rye has no committed pre-existing baseline for clean-checkout CI"
