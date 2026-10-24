---
name: security-review
description: Complete a security review of the pending changes on the current branch. Invoked by /security-review [<pr#>|<branch>|<path>|<range>].
disable-model-invocation: true
---

# /security-review

Run the review and show the result:

```python
result = await code_review.security_review("")   # the user's arguments, as typed
result
```

The call gathers the changes under review (the branch against its upstream
or default branch plus uncommitted work, or the PR number, branch, path or
range the user typed), then runs Claude Code's security review as a
workflow:

1. one subagent with the full security brief identifies candidate
   vulnerabilities (input validation, authentication and authorization,
   crypto and secrets, injection and code execution, data exposure), each
   with a severity, an exploit scenario and a fix;
2. one subagent per candidate filters false positives against the hard
   exclusions and precedents and scores it 1-10;
3. candidates scored below 8 drop.

The result's repr is the report: a table (`#`, location, category,
severity, finding) and one entry per finding with its exploit scenario and
the recommended fix, HIGH first. Show it to the user as it is; do not
re-derive or pad the findings. An empty table means nothing survived the
filter: say so in one line.

If the working directory is not a git repository, tell the user
`/security-review` needs to run inside one (and to `/cwd` into it if the
repository is in a subdirectory). Do not fix anything unless the user asks;
after fixes land, record them with `code_review.report_outcomes(...)`.
