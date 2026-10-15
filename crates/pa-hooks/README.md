# pa-hooks

Claude Code-compatible command hooks for Prime Agent. The `hooks` settings key runs user-configured commands on
session events, with Claude Code's JSON on stdin and its answers on stdout and exit code. Existing Claude Code hook
scripts work unmodified. The design and the user-facing contract are in `docs/hooks-and-plugins.md`.

## Scope

- **Settings:** `hooks` in `<agent dir>/settings.json`, then in a trusted project's `.prime/agent/settings.json`
  (`hooks` is a gated key, so an untrusted project contributes none). Settings are read once per session.
- **Events:**
  - `SessionStart`: the context rides the first admitted prompt.
  - `UserPromptSubmit`: adds context, or refuses the prompt.
  - `PreToolUse`: refuses a tool call, or a kernel host request.
  - `PostToolUse`: a note is appended to the result.
  - `Stop`: fire and forget.
- **Tool names hooks see:**
  - The model's own tool calls keep their names (`python_repl`, ...).
  - The kernel's `bash()` (`bash.run`) is `Bash` with `tool_input.command`.
  - The kernel's MCP calls (`mcp.session.call_tool`) are `mcp__<server>__<tool>`.
- **Matchers** are anchored regular expressions over the tool name; an empty or `*` matcher matches every tool.
- **Answers:**
  - Exit 2 refuses, with stderr as the reason.
  - JSON on stdout refuses with `permissionDecision: "deny"`, with `decision: "block"`, or with `continue: false`.
    `permissionDecision: "ask"` also refuses: there is no confirmation surface, and a policy must not fail open.
  - Additional context comes from `hookSpecificOutput.additionalContext`, or from plain stdout for `SessionStart`
    and `UserPromptSubmit`.
  - Any other exit, a timeout or a spawn failure is a reported, non-blocking error.
- **Process:** `sh -c` (`cmd /C` on Windows) in the session's cwd, with the host environment plus
  `CLAUDE_PROJECT_DIR`, `PRIME_AGENT_PROJECT_DIR` and `PRIME_AGENT_SESSION_ID`. The timeout is in seconds (default
  60). Output is capped at 64 KiB per stream. The matching commands of an event run in parallel, each once.

## Non-goals

- Claude Code's `prompt` hooks and its other events (`Notification`, `SubagentStop`, `PreCompact`, `SessionEnd`).
  These are reported as unsupported and skipped.
- `PostToolUse` for the kernel's `bash()`: no seam runs after a host request.
- `transcript_path`: always empty, because the conversation log belongs to the daemon.
- Interactive confirmation.

## Public API

- `HooksFeature`: the `SessionFeature` `pa-cli` installs (feature `hooks`).
- `config::{HooksConfig, HookEvent, HookCommand, DEFAULT_TIMEOUT}`.
- `run::{run, answer, HookAnswer, Outcome}`.
- `host_request_as_tool` and `FEATURE_NAME`.

## Seams it plugs into

`pa_core::features::SessionFeature`: `on_session_start`, `prompt_context`, `review_tool_call`,
`review_host_request`, `after_tool_call` and `on_agent_end`.

## Files it owns

None. It reads settings.

## Telemetry

None.

## Tests

- `src/config.rs`, `src/run.rs` and `src/lib.rs` cover matchers and skipped entries, every answer shape, a real
  command's stdin and timeout, and the host-request tool mapping.
- `tests/session.rs` uses POSIX shell hooks in real sessions:
  - start and submit context;
  - a refused prompt that leaves the start context for the next one;
  - a `PreToolUse` refusal and a `PostToolUse` note;
  - `Stop`'s input;
  - over a real kernel, a `Bash` hook denying `rm -rf` through `permissionDecision` while `echo` runs.
