---
name: ravo
description: Start the full RAVO loop (inspect, plan, implement, evaluate, diagnose, repair) over a continual harness mutation for a task from the Python REPL. Use when a harness change needs an evaluated search with repair rounds rather than one focused refine edit. Returns immediately; the run continues in the background and its progress is visible in the Agents View and via status().
---

# RAVO

RAVO runs a controlled search over a continual harness mutation proposal: it
inspects what in the harness and failure ledger relates to `task`, plans,
implements a proposal, evaluates it (structural screen, LLM judge, hygiene and
failure-ledger opponents), then diagnoses and repairs rejected proposals until
one is accepted or a limit is reached. Accepted proposals are applied to the
harness. The implementation lives in the host (the same one behind the user's
`/ravo` command); this skill is the kernel-side interface to it. Call it
directly from the Python REPL:

```python
await ravo.status()
await ravo.run("turn the deploy checklist we keep repeating into a skill")
await ravo.run("promote the retry policy to a global memory", global_=True, max_rounds=3)
await ravo.cancel()
```

## API

- `await ravo.run(task, instructions=None, global_=False, max_rounds=None, max_repairs=None)`
  — start a run. Returns `{"started": True, "runId": ...}` immediately, or
  `{"started": False, "reason": ...}` when a run is already in progress or
  RAVO is not available in this session. `task` is the harness change to
  search for; optional `instructions` add constraints. Set `global_=True` to
  target the global (cross-session) harness store; omit for local
  (session-scoped). `max_rounds` and `max_repairs` cap the loop.
- `await ravo.status()` — current run status as a dict (`runId`, `phase`,
  `round`, `repairs`, `stopReason`, `lastCertificate`, ...) or
  `{"phase": "idle"}` when nothing is running.
- `await ravo.cancel()` — request cancellation. Returns `{"cancelled": bool}`.

## Rules

- The run continues in the background; `run` never waits for it. Continue
  working normally and read `status()` when you need the outcome.
- Only one run per session at a time. Cancel or wait for the current run
  before starting another.
- Prefer `refine.run()` for a single focused memory, skill, prompt note, or
  subagent spec edit. Use RAVO when the change needs evaluation and repair
  rounds to get right.
