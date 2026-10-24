---
name: simplify
description: Review the changed code for reuse, simplification, efficiency, and altitude cleanups, then apply the fixes. Quality only - it does not hunt for bugs; use /code-review for that. Invoked by /simplify [<target>].
disable-model-invocation: true
---

# /simplify

`/simplify -> 4 cleanup agents in parallel -> apply the fixes`

You are improving the quality of the changed code, not hunting for bugs.
Review it for reuse, simplification, efficiency, and altitude issues, then
fix what you find. Do not look for correctness bugs - that is what
`/code-review` is for.

## Phase 1 - Review (4 cleanup agents in parallel)

```python
result = await code_review.simplify("")   # the user's target (PR number, branch, path or range), or ""
result
```

The call launches four independent review agents through `rlm.workflow`,
one per angle (reuse, simplification, efficiency, altitude), over the diff
(the branch against its base plus uncommitted work when no target is given).
Each finding has a `file`, `line`, a one-line `summary`, and the concrete
cost (what is duplicated, wasted, or harder to maintain).

## Phase 2 - Apply the fixes

Dedup findings that point at the same line or mechanism, and fix each
remaining one directly. Skip any finding whose fix would change intended
behavior, require changes well outside the reviewed diff, or that you judge
to be a false positive - note the skip rather than arguing with it. Then
call `code_review.report_outcomes({1: "fixed", 2: "skipped", ...})` with
every finding, and finish with a brief summary of what was fixed and what
was skipped (or confirm the code was already clean).
