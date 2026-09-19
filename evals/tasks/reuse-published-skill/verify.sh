set -e
[ -f slugs.txt ] || { echo "FAIL: no slugs.txt"; exit 1; }
exp=$'hello-world\nfoo-bar\na-b-c'
got=$(cat slugs.txt)
[ "$got" = "$exp" ] || { echo "FAIL: wrong slugs. got:"; echo "$got"; exit 1; }
# Behaviour: did it USE the published skill, or re-implement slugify? Score from structured events,
# not a grep of the whole stream (the word "evalslug" appears in any ls of the skills dir or in the
# model's prose). A use is an ipython tool call whose code imports evalslug or calls evalslug.run().
used=$(python3 - <<'PY'
import json, os
path = ".eval/events.jsonl"
used = False
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
        if e.get("toolName") != "ipython":
            continue
        code = (e.get("args") or {}).get("code") or ""
        if "import evalslug" in code or "from evalslug" in code or "evalslug.run(" in code:
            used = True
            break
print("yes" if used else "no")
PY
)
if [ "$used" = "yes" ]; then
  echo "PASS: correct output AND used the published evalslug skill"
else
  echo "FAIL: correct output but re-derived slugify instead of calling the published skill"
  exit 1
fi
