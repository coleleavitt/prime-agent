---
name: statusline
description: Set up the status line shown under the prompt (the statusLine setting), by default from the user's shell PS1. Invoked by /statusline [instructions].
disable-model-invocation: true
---

# /statusline

Create or update the `statusLine` command in the user's Prime Agent
settings (`~/.prime/agent/settings.json`, key `statusLine`). Any text after
the command is the user's instructions; with none, the task is "Configure my
statusLine from my shell PS1 configuration".

When asked to convert the user's shell PS1 configuration, follow these steps:

1. Read the user's shell configuration files in this order of preference:
   `~/.zshrc`, `~/.bashrc`, `~/.bash_profile`, `~/.profile`.
2. Extract the PS1 value using this regex pattern:
   `/(?:^|\n)\s*(?:export\s+)?PS1\s*=\s*["']([^"']+)["']/m`
3. Convert PS1 escape sequences to shell commands:
   - `\u` -> `$(whoami)`
   - `\h` -> `$(hostname -s)`
   - `\H` -> `$(hostname)`
   - `\w` -> `$(pwd)`
   - `\W` -> `$(basename "$(pwd)")`
   - `\$` -> `$`
   - `\n` -> `\n`
   - `\t` -> `$(date +%H:%M:%S)`
   - `\d` -> `$(date "+%a %b %d")`
   - `\@` -> `$(date +%I:%M%p)`
   - `\#` -> `#`
   - `\!` -> `!`
4. When using ANSI color codes, be sure to use `printf`. Do not remove
   colors. Note that the status line is printed in a terminal using dimmed
   colors.
5. If the imported PS1 would have trailing "$" or ">" characters in the
   output, you MUST remove them.
6. If no PS1 is found and the user did not provide other instructions, ask
   for further instructions.

## How the statusLine command runs

The command receives this JSON on stdin (Claude Code's shape, the fields
this client fills) and its stdout lines are shown under the prompt. It runs
when the session's model, token usage, cost, effort or vim mode change
(throttled to 300 ms, bounded by a 5 s timeout), and also every
`refreshInterval` seconds when that is set.

```
{
  "session_id": "string",
  "transcript_path": "string",          // the session file
  "cwd": "string",
  "model": { "id": "string", "display_name": "string" },
  "workspace": { "current_dir": "string", "project_dir": "string", "added_dirs": ["string"] },
  "version": "string",                  // Prime Agent's version
  "output_style": { "name": "string" },
  "cost": { "total_cost_usd": number, "total_duration_ms": number },
  "context_window": {                   // present once usage is known
    "total_input_tokens": number,
    "context_window_size": number,
    "used_percentage": number,
    "remaining_percentage": number
  },
  "exceeds_200k_tokens": boolean,
  "effort": { "level": "string" },      // when the model has a reasoning level
  "vim": { "mode": "INSERT" | "NORMAL" } // when vim mode is on
}
```

For anything beyond a one-liner, write a script at
`~/.prime/agent/statusline-command.sh` (made executable) that reads the JSON
with `jq` (or Python) and prints the line, and point the setting at it.

## Updating the setting

Read only the `statusLine` key of the settings file, never the whole file
into the conversation (it can hold credentials), and merge the new value in
place, keeping every other key:

```python
import json, pathlib
path = pathlib.Path.home() / ".prime/agent/settings.json"
settings = json.loads(path.read_text()) if path.exists() else {}
settings["statusLine"] = {"type": "command", "command": "~/.prime/agent/statusline-command.sh", "padding": 0}
path.write_text(json.dumps(settings, indent=2) + "\n")
```

Optional keys: `padding` (columns before each row), `refreshInterval`
(seconds), `hideVimModeIndicator` (when the command renders vim mode
itself). A `statusLine` in the project's `.prime/agent/settings.json`
overrides the user's; mention it if one exists.

Finally, tell the user what the status line will show, that it appears the
next time the client starts (`/quit` and run `prime-agent` again: the
session keeps running in the daemon), and that more changes can be asked
for the same way.
