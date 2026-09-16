set -e
[ -f solve.py ] || { echo "FAIL: no solve.py"; exit 1; }
out=$(python3 solve.py 2>&1) || { echo "FAIL: solve.py errored: $out"; exit 1; }
echo "$out" | grep -qE '(^|[^0-9])5([^0-9]|$)' || { echo "FAIL: expected median 5, got: $out"; exit 1; }
echo "PASS: median 5"
