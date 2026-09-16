set -e
python3 check.py >/dev/null 2>&1 || { echo "FAIL: count.py still wrong"; exit 1; }
if [ -n "$(git status --porcelain)" ]; then echo "FAIL: fix works but tree left dirty (no commit)"; exit 1; fi
n=$(git rev-list --count HEAD)
[ "$n" -ge 2 ] || { echo "FAIL: no commit created"; exit 1; }
echo "PASS: fixed and committed, tree clean ($n commits)"
