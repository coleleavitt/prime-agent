---
name: ctx
description: Manage this session's context window. Every user message and tool output carries a tag like §12§; drop outputs you no longer need with ctx.reduce, bring anything back with ctx.expand, check usage with ctx.status. Use when a context nudge appears, after you have taken what you need from a large output, or when you need detail from folded <session-history>.
---

# Context

Each user message and cell output in your context starts with a tag, `§N§`.
Old history is folded by a background historian into the `<session-history>`
message: compartments whose detail fades with age. Nothing is lost: the full
transcript stays on disk and `ctx.expand` returns it.

```python
await ctx.reduce("12-14,19")      # drop outputs you have used
print(await ctx.expand(13))       # one item's original text
print(await ctx.expand(start=40, end=55))  # raw transcript of a range
await ctx.status()                # usage, biggest outputs, queued drops
```

## API

- `await ctx.reduce(tags) -> dict`: queue drops for tags like `"3-5,9"` (an
  int, `"§12§"`, a list or a `range` work too). A dropped tool output renders
  as `[dropped §N§]`. Only tool outputs drop: the user's messages are kept
  (they carry the instructions), and the newest 3 tags are protected.
  Drops land on the next request whose prompt cache is cold anyway (first
  request after a restart or 5 minutes idle), at 65% usage, or after
  `ctx.flush()`. Returns `queued` and `skipped` (with reasons).
- `await ctx.expand(tag=None, start=None, end=None) -> str`: one item's
  original text (`tag`), or the raw transcript from tag `start` to tag `end`
  (about 15k tokens at most; narrow the range if it is cut).
- `await ctx.status() -> dict`: usage and window, biggest undropped outputs,
  queued drops, compartments, the historian's state, protected tags.
- `await ctx.flush() -> dict`: apply queued drops on the next request (costs
  one prompt-cache rebuild; use when usage is high).

## Habits

- Drop an output as soon as you have taken what you need from it; don't wait
  for the task to end. Copy what matters into your reply or a variable first.
- Drop the specific outputs you used, oldest first. Never blanket-drop a wide
  range.
- When `<session-history>` lacks a detail you need, `ctx.expand(start=, end=)`
  with the compartment's tags returns its raw transcript.
