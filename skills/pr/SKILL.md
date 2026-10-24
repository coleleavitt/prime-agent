---
name: pr
description: Open a GitHub pull request for the current branch with gh, a short title and a Summary / Test plan body. Invoked by /pr [guidance].
disable-model-invocation: true
---

# /pr

Any text after the command is the user's guidance for this PR.

## Context

Gather it in one cell before anything else. `<base>` is the repository's
default branch: `git symbolic-ref --short refs/remotes/origin/HEAD` without
the `origin/` prefix, else `main`.

```python
head = (await bash("git symbolic-ref --quiet --short refs/remotes/origin/HEAD")).output.strip()
base = head.removeprefix("origin/") or "main"
for command in (
    "git status",
    "git branch --show-current",
    f"git log --oneline origin/{base}..HEAD",
    f"git diff origin/{base}...HEAD",
):
    result = await bash(command)
    print(f"$ {command}\n{result.output}")
```

## Git Safety Protocol

- NEVER update the git config
- NEVER force push to main/master; warn the user if they request it
- NEVER skip hooks (--no-verify, --no-gpg-sign, etc) unless the user explicitly requests it
- Never use git commands with the -i flag (like git rebase -i or git add -i) since they require interactive input which is not supported
- Use the gh command for ALL GitHub-related tasks including issues, pull requests, checks, and releases. If given a GitHub URL, use gh to fetch it

## Your task

Based on the changes above, open a single pull request:

1. Analyze ALL changes that will be included in the PR (every commit since `<base>`, not just the latest), then draft a title and body:
   - Keep the title short (under 70 characters); put detail in the body
   - Follow any PR rules in the project's instruction files (AGENTS.md, CLAUDE.md)

2. Create a new branch if currently on `<base>`, push to remote with -u if needed, then create the PR. To ensure good formatting, ALWAYS pass the body inline via a HEREDOC, never from a file or stdin (`--body-file`/`-F`):

```
gh pr create --title "the pr title" --body "$(cat <<'EOF'
## Summary
<1-3 bullet points>

## Test plan
<a bulleted checklist of how the change was tested>
EOF
)"
```

3. Return the PR URL when you're done, so the user can see it.

Branch, push, and create the PR in a single cell. Do not run additional commands to read or explore code beyond the git context above. Uncommitted changes are not part of the PR: if there are any, say so (and offer `/commit`) instead of committing them yourself.
