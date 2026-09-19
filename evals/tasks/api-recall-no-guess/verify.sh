set -e
[ -f solve.py ] || { echo "FAIL: no solve.py"; exit 1; }
# Standard library only: reject a pandas/numpy shortcut by parsing the imports, not by running it.
stdlib_ok=$(python3 - <<'PY'
import ast, sys
ALLOWED = {
    "csv", "statistics", "sys", "os", "io", "math", "collections", "itertools",
    "functools", "pathlib", "decimal", "fractions", "operator", "re", "json", "heapq", "bisect", "array",
}
try:
    tree = ast.parse(open("solve.py").read())
except Exception as e:
    print("BADPARSE:" + str(e)); sys.exit(0)
bad = []
for node in ast.walk(tree):
    if isinstance(node, ast.Import):
        for n in node.names:
            if n.name.split(".")[0] not in ALLOWED:
                bad.append(n.name)
    elif isinstance(node, ast.ImportFrom):
        if node.module and node.module.split(".")[0] not in ALLOWED:
            bad.append(node.module)
print("BAD:" + ",".join(bad) if bad else "OK")
PY
)
case "$stdlib_ok" in
  OK) : ;;
  BAD:*) echo "FAIL: used a non-stdlib import (${stdlib_ok#BAD:})"; exit 1 ;;
  *) echo "FAIL: could not parse solve.py ($stdlib_ok)"; exit 1 ;;
esac
out=$(python3 -B solve.py 2>&1) || { echo "FAIL: solve.py errored: $out"; exit 1; }
# Exact output: the median is 3, and 3 is not the mean (22) or the row count (5), so only the right
# statistic passes. Accept a lone "3", "3.0", or a trailing "...: 3" line, nothing else.
ans=$(printf '%s' "$out" | tr -d '[:space:]')
case "$ans" in
  3|3.0|*:3|*:3.0|*=3|*=3.0) : ;;
  *) echo "FAIL: expected median 3, got: $out"; exit 1 ;;
esac
# Guard metric: an AttributeError / "has no attribute" from ipython is the model guessing an API
# that does not exist (statistics.median exists; statistics.median_of does not). A recall-using run
# produces zero of these.
guesses=$(python3 - <<'PY'
import json, os
path = ".eval/events.jsonl"
n = 0
if os.path.exists(path):
    for line in open(path, errors="replace"):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            e = json.loads(line)
        except Exception:
            continue
        if e.get("type") != "tool_execution_end":
            continue
        if e.get("toolName") != "ipython" or not e.get("isError"):
            continue
        blob = json.dumps(e.get("result") or "")
        if "AttributeError" in blob or "has no attribute" in blob:
            n += 1
print(n)
PY
)
[ "${guesses:-0}" -eq 0 ] || { echo "FAIL: correct output but guessed a non-existent API ${guesses} time(s)"; exit 1; }
echo "PASS: median 3, stdlib only, no API guessing"
