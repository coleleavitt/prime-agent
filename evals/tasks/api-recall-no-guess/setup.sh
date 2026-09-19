set -e
# median 3, mean 22, count 5 — all different, so a printed mean, row count or wrong statistic can
# no longer pass as "the median".
printf 'name,value\na,1\nb,2\nc,3\nd,4\ne,100\n' > data.csv
# A git repo so the cross-session resolution index has somewhere to key its API-resolution hints.
git init -q .
printf '.eval/\n' > .gitignore
git add -A && git commit -qm "initial"
git rev-parse HEAD > .git/eval-setup-sha
