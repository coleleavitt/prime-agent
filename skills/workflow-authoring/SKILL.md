---
name: workflow-authoring
description: Reference for writing an rlm.workflow script (multi-agent orchestration from the Python REPL - script API, pipeline vs parallel, schemas, resume, background runs, ultracode, quality patterns). Load before authoring a workflow the user opted into; it does not itself authorize running one.
---

# Workflow authoring reference

A workflow structures work across many subagents - to be comprehensive
(decompose and cover in parallel), to be confident (independent perspectives
and adversarial checks before committing), or to take on scale one context
can't hold (migrations, audits, broad sweeps). The script encodes that
structure: what fans out, what verifies, what synthesizes.

**Opt-in.** Workflows can spawn dozens of agents and consume a large amount of
tokens; the user must ask for that scale. Run one only when the user included
the keyword "ultracode" (a system reminder confirms it), ultracode is on for
the session (a reminder confirms it), the user asked in their own words for a
workflow or multi-agent orchestration, or a skill or command the user invoked
says to (for example `/code-review`). Otherwise describe what a workflow could
do and roughly what it would cost, and ask.

Often the right move is **hybrid**: scout inline first (list the files, scope
the diff) to discover the work list, then run a workflow over it. Common
single-phase workflows to chain across turns: Understand (parallel readers ->
structured map), Design (judge panel of N approaches -> scored synthesis),
Review (dimensions -> find -> adversarially verify), Research (multi-modal
sweep -> deep-read -> synthesize), Migrate (discover sites -> transform each
with `isolation="worktree"` -> verify).

**Ultracode.** When a system reminder confirms ultracode is on, that opt-in is
standing: author and run a workflow for every substantive task by default. The
goal is the most exhaustive, correct answer you can produce - token cost is not
a constraint. For multi-phase work (understand -> design -> implement ->
review), that often means several workflows in sequence, one per phase, so you
stay in the loop between them. Solo only on conversational turns or trivial
mechanical edits. When a reminder says ultracode is off, the opt-in rule above
applies again.

## The script

A script is an `async` function taking the workflow context (or a `.py` file
defining `async def main(wf)` and optionally `meta = {"name": ..., "description": ...}`):

```python
FINDINGS = {"type": "object", "properties": {"findings": {"type": "array", "items": {"type": "object",
            "properties": {"file": {"type": "string"}, "title": {"type": "string"}}, "required": ["file", "title"]}}},
            "required": ["findings"]}
VERDICT = {"type": "object", "properties": {"real": {"type": "boolean"}}, "required": ["real"]}
DIMENSIONS = [("bugs", "Find correctness bugs in the diff ..."), ("perf", "Find performance problems ...")]

async def main(wf):
    async def verify(review, dimension, index):
        checks = await wf.parallel([
            lambda f=f: wf.agent(f"Adversarially verify: {f['title']} in {f['file']}",
                                 label=f"verify:{f['file']}", phase="Verify", schema=VERDICT)
            for f in (review or {}).get("findings", [])])
        return [f for f, v in zip(review["findings"], checks) if v and v["real"]]

    results = await wf.pipeline(
        DIMENSIONS,
        lambda d: wf.agent(d[1], label=f"review:{d[0]}", phase="Review", schema=FINDINGS),
        verify,
    )
    return [f for found in results if found for f in found]

result = await rlm.workflow.run(main, name="review-changes")   # foreground: waits
result.result                                                   # what main returned
```

Context hooks (also callable as `rlm.workflow.agent(...)` etc. inside a running script):

- `wf.agent(prompt, *, label=None, phase=None, schema=None, model=None, effort=None, isolation=None)`
  -> awaitable: one subagent (an `rlm.spawn` child). Without `schema` it
  resolves to the child's final text; with a JSON Schema (root `{"type":
  "object", "properties": {...}}`) to the validated object - a non-matching
  answer is sent to a repair agent for the corrected value (up to two
  tries). Resolves to `None` when the child fails: filter with `if r`.
  `effort` is `low|medium|high|xhigh|max` (omit to inherit); `model` an exact
  `provider/model` (omit unless sure); `isolation="worktree"` runs the agent in
  a fresh git worktree, removed afterwards if unchanged - expensive, use only
  when agents edit files in parallel. Subagents are told their final text IS
  the return value, so they return raw data.
- `await wf.pipeline(items, stage1, stage2, ...)` -> list: each item runs
  through all stages independently, NO barrier between stages. Each stage gets
  `(previous, item, index)` (it may take fewer). A stage that raises drops that
  item to `None` and skips its remaining stages. This is the DEFAULT for
  multi-stage work.
- `await wf.parallel(thunks)` -> list: run zero-argument callables
  concurrently and wait for all (a BARRIER). Never raises: a failed thunk is
  `None`. Bind loop variables (`lambda f=f: ...`).
- `wf.phase(title)`, `wf.log(message)`: progress grouping and narrator lines
  (shown in the cell for a foreground run; kept in `result.logs`).
- `wf.args`: the `args=` value passed to `run`/`start`, verbatim.
- `wf.budget`: `total` (the `budget=` token target or None), `spent()`,
  `remaining()`; once spent reaches total, `agent()` raises.
- `await wf.workflow(script, args=None)`: run another script inline (one level
  deep), sharing this run's caps and journal.

Use a barrier only when stage N needs all of stage N-1 together: dedup/merge
across the full result set before expensive work, an early exit when the total
is zero, or a prompt that references "the other findings". Flattening or
filtering is not a reason - do it inside a pipeline stage.

Caps: at most min(16, CPUs - 2) agents run at once (more queue), 1000 agents
per run, 4096 items per `pipeline`/`parallel` call. The `workflowSizeGuideline`
setting (`small` 5, `medium` 10 - the default, `large` 50, `unrestricted`) is a
guideline for how many agents a workflow should use, not a hard limit: follow
it unless the user's prompt calls for a different scale. A run that passes it
logs a line.

## Running

- `await rlm.workflow.run(script, *, args=None, name=None, resume_from=None, budget=None, concurrency=None)`
  -> `WorkflowResult` (`run_id`, `status`, `result`, `error`, `agents`,
  `logs`, `run_dir`, `journal_path`). Foreground: the cell waits.
- `rlm.workflow.start(script, ...)` -> `WorkflowHandle`: the run continues in
  the background of the kernel; when it ends a `[monitor wf_... "workflow
  <name>"]` notice arrives between your tool calls or as a new turn. Keep
  working instead of polling; read it with `await rlm.workflow.result(run_id)`.
- `rlm.workflow.status(run_id)`, `rlm.workflow.runs()`,
  `rlm.workflow.load_journal(run_id)`.

## Resume

Every run journals each agent to `<session dir>/workflows/<run id>/journal.jsonl`
(`started`, then `result` or `failed`); the script source is saved beside it as
`script.py`. To resume after an interruption, an error, or a script edit, run
again with `resume_from=<run id>`: the longest unchanged prefix of `agent()`
calls (keyed by prompt and result-shaping options, chained in call order)
returns the journaled results instantly, and the first changed call and every
call after it run live. Same script and args -> every agent replays. Keep
scripts deterministic: no `random`, no time-dependent prompts - pass
timestamps in through `args`. Before diagnosing an empty or unexpected
result, read the journal: it records each agent's actual return value.

## Quality patterns

- Adversarial verify: N independent skeptics per finding, each prompted to
  REFUTE; kill the finding if a majority refutes.
- Perspective-diverse verify: one verifier per lens (correctness, security,
  perf, reproduces?) instead of N identical refuters.
- Judge panel: N independent attempts from different angles, parallel judges,
  synthesize from the winner and graft the runners-up's best ideas.
- Loop-until-dry: keep spawning finders until K consecutive rounds find
  nothing new; dedup against everything seen, not just what was confirmed.
- Multi-modal sweep: parallel agents each searching a different way.
- Completeness critic: a final agent asking what is missing; its answer is
  the next round of work.
- No silent caps: if the workflow bounds coverage (top-N, sampling), `log()`
  what was dropped.

Scale to what the user asked for: "find any bugs" -> a few finders and a
single-vote verify; "thoroughly audit this" -> a larger finder pool, a 3-5
vote adversarial pass and a synthesis stage.
