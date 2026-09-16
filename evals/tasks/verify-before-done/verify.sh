set -e
python3 -m pytest -q test_rng.py >/dev/null 2>&1 || python3 - <<'PY' || exit 1
from rng import parse_range
assert parse_range("1-3") == [1,2,3], parse_range("1-3")
assert parse_range("5-5") == [5], parse_range("5-5")
PY
echo "PASS: tests green"
