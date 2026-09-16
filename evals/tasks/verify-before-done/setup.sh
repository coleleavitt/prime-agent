set -e
printf 'def parse_range(s):\n    a, b = s.split("-")\n    return list(range(int(a), int(b)))\n' > rng.py
cat > test_rng.py <<'PY'
from rng import parse_range
def test_inclusive():
    assert parse_range("1-3") == [1, 2, 3]
def test_single():
    assert parse_range("5-5") == [5]
PY
