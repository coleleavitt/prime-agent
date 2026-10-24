---
name: run
description: Launch and drive this project's app to see a change working. Use when asked to run, start, or screenshot the app, or to confirm a change works in the real app (not just tests). First looks for a project skill that already covers launching the app; otherwise falls back to built-in patterns per project type (CLI, server, TUI, library). Invoked by /run.
disable-model-invocation: true
---

**Running means launching the actual app and interacting with it** - not
the test suite, not an `import` of an internal function and a `print`. The
app as a user (human or programmatic) would meet it: the CLI at its command,
the server at its socket, the GUI at its window.

## First: does a project skill already cover this?

A project skill that launches this app is the repo's verified path - its
author already cold-started and committed what worked: the exact install
line, the env vars, the patches, the driver. Use it instead of
rediscovering. Project skills live in `.prime/agent/skills/` and
`.claude/skills/`, at the repository root and in the package directories:

```python
print((await bash(r'''d=$PWD; while :; do
  grep -Hm1 '^description:' "$d"/.prime/agent/skills/*/SKILL.md "$d"/.claude/skills/*/SKILL.md 2>/dev/null
  [ -e "$d/.git" ] || [ "$d" = / ] && break
  d=$(dirname "$d")
done''')).output)
```

- **One describes launching/driving this app** -> read that SKILL.md and
  follow it verbatim. Don't paraphrase; don't skip the patches.
- **Mega-repo, several plausible, no clear match** -> ask the user which
  unit to run.
- **Stale** (fails on mechanics unrelated to your task) -> tell the user;
  offer to refresh it.
- **Nothing about running** -> fall back to the patterns below.

## Otherwise: match the shape, use the pattern

Pick the row closest to your project. Each example walks through launch +
first interaction; ignore any trailing "write the skill" section - you're
using the recipe, not authoring one.

| Project type | Handle | Example |
|---|---|---|
| CLI tool | direct invocation, exit code, stdin/stdout | [examples/cli.md](examples/cli.md) |
| Web server / API | background launch + `curl` smoke | [examples/server.md](examples/server.md) |
| TUI / interactive terminal | tmux `send-keys` / `capture-pane` | [examples/tui.md](examples/tui.md) |
| Library / SDK | import-and-call smoke script at the package boundary | [examples/library.md](examples/library.md) |

If nothing fits, start from the closest match and adapt. For a web app,
start its dev server like the server example and drive a headless browser
(Playwright, if the project has it) to a screenshot; for a desktop app, drive
it under a virtual display and take a screenshot. **Look at the screenshot**
(`attach_image`): a blank frame is a failure to launch.

Long-running processes run as `bash()` handles in the background: keep the
handle, poll for readiness (never a fixed sleep), and stop what you started
by its handle or its port, never with a broad `pkill -f` pattern that can
match this session's own processes.

## Drive it, don't just launch it

Launching with no interaction proves the entrypoint resolves. That's not
running the app - it's typechecking with extra steps. Drive it to a point
where a user would see something:

- CLI -> type a representative command, check the exit code and output.
- Server -> hit the route the diff touches with `curl`, read the body.
- TUI -> `send-keys` a navigation, `capture-pane` the result.
- GUI -> click the button, screenshot the window.

If the fallback pattern didn't work out of the box - you had to install
packages, set env vars, patch config, or write a driver - recommend
capturing that work as a project skill (`.prime/agent/skills/run-<app>/`,
the skill-creator skill knows the format) in your report. If it just worked,
don't.
