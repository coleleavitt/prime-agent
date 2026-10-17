---
name: advisor
description: Ask a stronger reviewer model for guidance from the Python REPL. It sees this conversation (the task, your cells and results, your replies). Use before committing to an approach on a multi-step task, when stuck (recurring errors, an approach not converging), when considering a change of approach, and before declaring a long task done.
---

# Advisor

The advisor is a stronger reviewer model the user configured. It reads this
session's conversation, so ask about what you are deciding, not what you did:

```python
advice = await advisor.ask("I plan to patch the parser instead of the lexer; which constraint breaks the tie?")
print(advice)
```

## API

- `await advisor.ask(question) -> str`: the advisor's guidance. Raises when the
  question is empty or longer than 8000 characters, or when the advisor model
  fails.

## When to ask

- Before substantive work: before writing, before committing to an
  interpretation, before building on an assumption. Orientation first (finding
  files, reading what is there) is fine; then ask.
- When you believe a long task is complete: make the result durable first
  (write the file, commit the change), then ask.
- When stuck: errors recurring, an approach not converging, results that do
  not fit.
- When considering a change of approach.

On short reactive tasks where the next step follows from the output you just
read, you do not need to ask.

## Weighing the advice

Give the advice serious weight. If a step it suggests fails empirically, or you
have primary-source evidence against a specific claim (the file says X), adapt.
If your evidence points one way and the advisor another, do not switch
silently: ask once more with both ("I found X, you suggest Y, which constraint
breaks the tie?").
