#!/usr/bin/env bash
set -euo pipefail
HERE=$(cd -- "$(dirname -- "$0")" && pwd)
BIN=$(mktemp "${TMPDIR:-/tmp}/historical-eventbus.XXXXXX")
trap 'rm -f "$BIN"' EXIT
rustc --edition 2024 "$HERE/historical-eventbus-deadlock.rs" -o "$BIN"
set +e
timeout --signal=TERM --kill-after=2s 8s setsid strace -f -e trace=futex -o "$HERE/historical-futex.strace" "$BIN" >"$HERE/historical.stdout" 2>"$HERE/historical.stderr" &
WRAPPER=$!
sleep 1
CHILD=$(pgrep -P "$WRAPPER" | head -1)
{
  echo "wrapper_pid=$WRAPPER child_pid=${CHILD:-missing}"
  if [[ -n "${CHILD:-}" ]]; then
    ps -L -p "$CHILD" -o pid,ppid,pgid,sid,tid,stat,wchan:32,comm
    if [[ -r "/proc/$CHILD/status" ]]; then
      grep -E '^(Name|Pid|PPid|NSpgid|NSsid|Threads):' "/proc/$CHILD/status"
    fi
  fi
} >"$HERE/historical-process-state.txt"
wait "$WRAPPER"
STATUS=$?
set -e
printf '%s\n' "$STATUS" >"$HERE/historical-exit-code.txt"
if [[ "$STATUS" != 124 && "$STATUS" != 137 ]]; then
  echo "historical reproduction did not hit the timeout deadline: $STATUS" >&2
  exit 1
fi
RYE_ROOT=${RYE_ROOT:-/home/cole/RustProjects/active/rye}
# Compile outside the deadline. The deadline measures only test execution.
cargo test --manifest-path "$RYE_ROOT/Cargo.toml" -p rye-agent-core --test execution_primitives --no-run >"$HERE/current-control-build.stdout" 2>"$HERE/current-control-build.stderr"
timeout 8s cargo test --manifest-path "$RYE_ROOT/Cargo.toml" -p rye-agent-core --test execution_primitives event_bus_continues_watcher_delivery_after_listener_failure_and_reports_it -- --exact --nocapture >"$HERE/current-control.stdout" 2>"$HERE/current-control.stderr"
echo 0 >"$HERE/current-control-exit-code.txt"
node "$HERE/verify-record-joins.mjs"
