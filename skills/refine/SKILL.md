---
name: refine
description: Trigger continual harness refinement from the Python REPL. Use when you notice a repeated failure, reusable tactic, delegation role, or behavior policy that should be persisted as a harness entry. Returns immediately; refinement runs when the current turn ends.
---

# Refine

Refinement analyzes the conversation trajectory and applies small, evidence-backed
updates to the continual harness (prompts, memories, skills, subagent specs).
The implementation lives in the host (the same one behind the user's `/refine`
command); this skill is the kernel-side interface to it. Call it directly from
the Python REPL:

```python
await refine.status()
await refine.run()
await refine.run("create a memory about always checking git status before committing")
await refine.run("promote the error-handling pattern to a global skill", global_=True)
plan = await refine.preview("record the flaky-test workaround")
await refine.run(plan_id=plan["plan_id"])  # apply exactly the previewed plan
```

## API

- `await refine.status()` — current refine state as a dict: `pending` (whether a
  requested refine is already queued for this turn), `in_flight` (whether a
  refine is currently planning or applying), and `preview_ids` (previewed plans
  still held).
- `await refine.preview(instructions=None, global_=False)` — plan now and return
  the proposal (`plan_id`, `summary`, `rationale`, `expected_outcome`, `scope`,
  `edits`) without scheduling or writing anything. Use it when the user wants to
  approve a refinement before it lands.
- `await refine.run(instructions=None, global_=False, plan_id=None)` — schedule refinement.
  Returns `{"scheduled": True}` immediately, or `{"scheduled": False, "reason": ...}`
  when refinement cannot start. Optional `instructions` focus the refinement on a
  specific observation. Set `global_=True` to target the global harness store
  (cross-session); omit for local (session-scoped) refinement. Pass
  `plan_id=` from `refine.preview()` to apply exactly that plan instead of
  re-planning; a plan whose entries changed since the preview is refused, and
  previews expire once a refinement or compaction runs.

## Rules

- Refinement never runs mid-cell. A scheduled refinement runs when the current
  turn ends; the harness applies changes and rebuilds the system prompt, then
  resumes you automatically. Continue working normally after calling it.
- One request per turn is enough; calling `run` again before the turn ends only
  updates the instructions.
- Use refinement after observing a repeated failure, a reusable tactic, a
  repeated delegation role, or a behavior policy worth persisting. Do not
  rewrite the whole harness when a focused memory, skill, prompt note, or
  subagent spec is enough.
