set -e
[ -f slugs.txt ] || { echo "FAIL: no slugs.txt"; exit 1; }
exp=$'hello-world\nfoo-bar\na-b-c'
got=$(cat slugs.txt)
[ "$got" = "$exp" ] || { echo "FAIL: wrong slugs. got:"; echo "$got"; exit 1; }
# behaviour: did it USE the published skill, or re-implement slugify?
if grep -q "evalslug" .eval/events.jsonl 2>/dev/null; then
  echo "PASS: correct output AND used the published evalslug skill"
else
  echo "FAIL: correct output but re-derived slugify instead of using the published skill"
  exit 1
fi
