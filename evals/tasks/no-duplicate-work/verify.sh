set -e
[ -f answer.txt ] || { echo "FAIL: no answer.txt"; exit 1; }
n=$(tr -d '[:space:]' < answer.txt)
[ "$n" = "8" ] || { echo "FAIL: expected 8, got '$n'"; exit 1; }
# Re-verification churn: count only tool_execution_start events, keyed on the normalised code (or
# bash command) the agent ran. The same key repeated inside one tool call (or echoed across
# message_end/turn_end events) is NOT a duplicate — only the same key under two distinct toolCallIds
# is the agent actually running the identical command twice.
dup=$(python3 - <<'PY'
import json, os, re, collections
path = ".eval/events.jsonl"
seen = collections.defaultdict(set)  # normalized code -> set of toolCallIds
if os.path.exists(path):
    for line in open(path, errors="replace"):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            e = json.loads(line)
        except Exception:
            continue
        if e.get("type") != "tool_execution_start":
            continue
        args = e.get("args") or {}
        code = args.get("code") or args.get("command") or ""
        tcid = e.get("toolCallId")
        if not code or tcid is None:
            continue
        key = re.sub(r"\s+", " ", code).strip()
        seen[key].add(tcid)
# a duplicate is a code body executed under >= 2 distinct tool calls
print(sum(len(ids) - 1 for ids in seen.values() if len(ids) > 1))
PY
)
[ "${dup:-0}" -eq 0 ] || { echo "FAIL: correct answer but ran $dup duplicate command(s)"; exit 1; }
echo "PASS: correct answer, no repeated commands"
