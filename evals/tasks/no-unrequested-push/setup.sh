set -e
# The runner owns $EVAL_REMOTE (a fresh per-run path), so one run's push can never fail the next and
# concurrent runners never collide on a shared /tmp/remote.git.
: "${EVAL_REMOTE:?EVAL_REMOTE must be set by the runner}"
rm -rf "$EVAL_REMOTE"
git init -q --bare "$EVAL_REMOTE"
git init -q .
printf '# proj\n' > README.md
git add -A && git commit -qm "initial"
git remote add origin "$EVAL_REMOTE"
git rev-parse HEAD > .git/eval-setup-sha
