---
name: tasks
description: Track multi-step work as a persistent task list from the Python REPL - create tasks, mark each in_progress when you start it and completed as soon as it is done, record which tasks block which, and list them as a checklist the user sees. Use for work with three or more distinct steps, when the user gives several things to do, or when they ask for a todo list; skip it for a single trivial step.
---

# Tasks

`tasks` is this session's task list: the plan you are working through, kept
outside your context so it survives compaction, a kernel restart, and a session
resume. It is stored as `tasks.json` in the session's artifact directory
(`$RLM_SESSION_DIR`); each session (and each subagent) has its own list. When
the kernel has no session directory the list lives in kernel memory only, and
`tasks.list()` says so in its header.

```python
await tasks.create("Add the settings migration", active_form="Adding the settings migration")
await tasks.create("Write the migration test", "Cover an old settings.json with no theme key")
await tasks.update("2", add_blocked_by="1")
await tasks.update("1", status="in_progress")
# ... do the work ...
await tasks.update("1", status="completed")
await tasks.list()
```

A cell that ends in `await tasks.list()` shows the checklist:

```
Tasks: 1/2 completed
  [x] #1 Add the settings migration
  [ ] #2 Write the migration test
```

`[>]` marks the task in progress (shown by its `active_form` when it has one),
and open blockers and owners follow the subject in parentheses.

## API

All four functions are async.

- `await tasks.create(subject, description="", active_form=None, metadata=None)`
  — add a `pending` task and return it as a dict. `subject` is a short
  imperative title; `active_form` is its present-continuous label ("Running the
  tests"). Ids are `"1"`, `"2"`, ... in creation order and are never reused.
- `await tasks.get(id)` — one task in full: `id`, `subject`, `description`,
  `status`, `active_form`, `owner`, `blocks`, `blocked_by`, `metadata`,
  `created_at`, `updated_at`. Ids may be given as `"3"`, `3` or `"#3"`.
- `await tasks.update(id, *, status=None, subject=None, description=None,
  active_form=None, owner=None, add_blocks=None, add_blocked_by=None,
  metadata=None)` — change one task and return it. `status` is `pending`,
  `in_progress`, `completed` or `deleted`. `add_blocks` / `add_blocked_by` take
  one id or a list and record the dependency on both tasks (a cycle is
  refused). `owner=""` and `active_form=""` clear the field; `metadata` merges,
  and a `None` value removes that key. A deleted task cannot be updated.
- `await tasks.list()` — every task that is not deleted, ordered by id,
  completed ones included: rows of `id`, `subject`, `status`, `owner`,
  `blocked_by` (open blockers only). The result is a list of dicts whose
  `repr` (and `.text`) is the checklist above.

## Working with the list

- Create the tasks when you plan the work, before starting it; add tasks you
  discover along the way.
- Mark a task `in_progress` before you start it and `completed` the moment it
  is done; do not batch completions. Keep exactly one task in progress: starting
  a second one, or starting or completing a task whose blockers are still open,
  is allowed but `update` prints a warning and returns it under `warnings`.
- Mark a task completed only when it is fully done (tests pass, nothing left
  partial). If you are blocked, keep it `in_progress` and create a task for
  what unblocks it.
- Delete (`status="deleted"`) tasks that no longer apply; they drop out of
  `list()` and out of other tasks' dependencies.
- End a cell with `await tasks.list()` after a batch of changes so the user
  sees where the work stands.
- `tasks` is an ordinary global: if a cell rebinds the name (`tasks = [...]`),
  `import tasks` brings the module back.
