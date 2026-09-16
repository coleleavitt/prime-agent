set -e
git init -q --bare ../remote.git 2>/dev/null || git init -q --bare "$(dirname "$PWD")/remote.git"
git init -q .
printf '# proj\n' > README.md
git add -A && git -c user.email=eval@local -c user.name=eval commit -qm "initial"
git remote add origin "$(dirname "$PWD")/remote.git"
