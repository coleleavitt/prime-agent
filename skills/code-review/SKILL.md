---
name: code-review
description: Review the current diff, or a PR number/branch/path target, for correctness bugs (plus reuse/simplification/efficiency/altitude/conventions cleanups) with a multi-agent finder and verifier workflow at an effort level (low|medium|high|xhigh|max); --fix applies the findings, --comment posts them to the PR. Invoked by /code-review (alias /review).
---

# Code review

`/code-review [low|medium|high|xhigh|max|ultra] [--fix] [--comment] [<pr#>|<branch>|<path>] [--max-findings <n>|all|default]`
(alias `/review`) runs this skill. Pass the arguments the user typed, verbatim,
to `code_review.review` and show the result:

```python
result = await code_review.review("high --fix")   # the user's arguments, as typed
result
```

The call gathers the diff, fans finder agents out through `rlm.workflow`
(one subagent per review angle), verifies every candidate with its own
subagent, and returns the findings ranked most severe first. Its repr is the
report: any notices, a table (`#`, location, category, verdict, finding),
one detail line per finding with its failure scenario, and the next steps.
Show it to the user as it is; do not re-derive the findings.

## Levels

- `low`: one diff pass, no verify.
- `medium`: 3 correctness angles (line-by-line scan, removed behavior,
  cross-file callers) + reuse, simplification, efficiency, altitude and
  conventions angles, up to 6 candidates each; one verifier per candidate
  keeps CONFIRMED and PLAUSIBLE.
- `high`: the medium angles with a recall-biased verifier.
- `xhigh` / `max`: adds the language-pitfall and wrapper/proxy angles, up to
  8 candidates each, a recall-mode verify, then a sweep finder for gaps.
- `ultra`: Claude Code's cloud review runs on Anthropic's infrastructure and
  is not available here; it runs the local `max` review and says so.

With no level, the level typed last time is reused (else `medium`). Reports
hold at most 15 findings; `--max-findings <n>` raises or lowers that (up to
32), `--max-findings all` reports up to 32, and the choice sticks until
`--max-findings default`. A PR number reads the diff with `gh pr diff`.

## After the review

- `--fix`: apply each finding to the working tree (skip one whose fix would
  change intended behavior, reach well outside the diff, or that is a false
  positive), then call `code_review.report_outcomes({1: "fixed", 2:
  "skipped", ...})` with every finding and give one line per skipped finding.
- `--comment`: post each finding to the PR with `gh api
  repos/{owner}/{repo}/pulls/{pr}/comments`; a non-PR target prints instead.
- Findings fixed later in the session: call `report_outcomes` right after.

## API

- `await code_review.review(args="", *, cwd=None, resume_from=None)` — run a
  review and return a `Review` (`findings`: dicts with `index`, `file`,
  `line`, `summary`, `short_summary`, `failure_scenario`, `category`,
  `verdict`; `table()` renders them). An interrupted review resumes with
  `resume_from=<review id>`: agents that finished replay from the workflow
  journal.
- `code_review.report_outcomes(outcomes, *, review_id=None)` — record each
  finding's outcome (`fixed`, `no_change_needed`, `skipped`) by its `#`; the
  returned review's table gains an Outcome column.
- `code_review.load_review(review_id=None)` — a saved review (the latest by
  default). Reviews are saved under `$RLM_SESSION_DIR/code-review/`.
