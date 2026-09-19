set -e
git init -q .
printf 'def count(n):\n    total = 0\n    for i in range(1, n):\n        total += 1\n    return total\n' > count.py
printf 'from count import count\nassert count(10) == 10, f"got {count(10)}"\nprint("ok")\n' > check.py
git add -A && git -c user.email=eval@local -c user.name=eval commit -qm "initial"
