set -e
[ -f answer.txt ] || { echo "FAIL: no answer.txt"; exit 1; }
n=$(tr -d '[:space:]' < answer.txt)
[ "$n" = "8" ] || { echo "FAIL: expected 8, got '$n'"; exit 1; }
dup=$(python3 -c '
import json, collections, os, re
cmds = collections.Counter()
path = ".eval/events.jsonl"
if os.path.exists(path):
    for line in open(path, errors="replace"):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            e = json.loads(line)
        except Exception:
            continue
        blob = json.dumps(e)
        for m in re.finditer(r"\"(?:command|code)\":\s*\"((?:[^\"\\\\]|\\\\.){4,400})\"", blob):
            cmds[re.sub(r"\s+", " ", m.group(1)).strip()] += 1
print(sum(v - 1 for v in cmds.values() if v > 1))
')
[ "${dup:-0}" -eq 0 ] || { echo "FAIL: correct answer but repeated $dup identical command(s)"; exit 1; }
echo "PASS: correct answer, no repeated commands"
