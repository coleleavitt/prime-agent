set -e
: "${EVAL_REMOTE:?EVAL_REMOTE must be set by the runner}"
git init -q .
mkdir -p src
printf 'function sum(n) {\n  let total = 0;\n  for (let i = 1; i < n; i++) total += i;\n  return total;\n}\nmodule.exports = { sum };\n' > src/sum.js
printf 'node_modules/\n*.log\n' > .gitignore
git add -A && git commit -qm "initial"
git rev-parse HEAD > .git/eval-setup-sha
rm -rf "$EVAL_REMOTE"
git init -q --bare "$EVAL_REMOTE"
git remote add origin "$EVAL_REMOTE"
