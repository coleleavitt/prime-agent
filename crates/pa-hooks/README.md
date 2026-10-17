# pa-hooks

Claude Code-compatible command hooks for Prime Agent. The `hooks` settings key runs user-configured commands on
session events, with Claude Code's JSON on stdin and its answers on stdout and exit code. Existing Claude Code hook
scripts work unmodified. The design and the user-facing contract are in `docs/hooks-and-plugins.md`.

## Scope

- **Settings:** `hooks` in `<agent dir>/settings.json`, then in a trusted project's `.prime/agent/settings.json`
  (`hooks` is a gated key, so an untrusted project contributes none). Settings are read once per session.
- **Input:** every event gets Claude Code's base fields (`session_id`, `transcript_path` (empty), `cwd`,
  `hook_event_name`, `permission_mode` (`default`)) plus its own:

  | event | when | its own fields | what its answer does |
  |---|---|---|---|
  | `SessionStart` | the session's engine is built | `source` (`startup`/`resume`) | context rides the first admitted prompt |
  | `UserPromptSubmit` | a prompt is admitted | `prompt` | adds context, or refuses the prompt |
  | `PreToolUse` | a tool call or kernel host request is about to run | `tool_name`, `tool_input` | refuses it, or asks the user |
  | `PostToolUse` | a tool call ran; a kernel `bash()` command's result arrived (`kernel_bash`) | `tool_name`, `tool_input`, `tool_response` | a note on the (cell's) result |
  | `PostToolBatch` | a tool batch finished, before the next model request | `tool_calls: [{tool_name, tool_use_id}]` | context (and `block` reasons, as feedback) rides that request |
  | `Stop` | the model's final turn ended and nothing queued continues the run | `stop_hook_active`, `last_assistant_message`, `background_tasks: [{id, type, status, description, command?}]` | `block`/exit 2 keeps the agent going with the reason |
  | `SubagentStart` | `rlm.run` spawned a child agent | `agent_id`, `agent_type` (the child's name) | none (fire and forget) |
  | `SubagentStop` | the session learns a child settled (`rlm.collect`, `rlm.list_subagents`, a terminal-notice row), once per child | `agent_id`, `agent_type`, `stop_hook_active` (false), `last_assistant_message` and `agent_session_dir` when known | none |
  | `PreCompact` | before a compaction | `trigger` (`manual`/`auto`), `custom_instructions` | plain stdout or `additionalContext` is added to the summary instructions |
  | `PostCompact` | after a compaction committed | `trigger`, `compact_summary` | none |
  | `Notification` | a background completion notice reaches the model (`[bash-done ...]`, a child's terminal notice); a `PreToolUse` hook asks the user | `message`, `title`, `notification_type` (`background_task_completed`, `permission_prompt`) | none |
  | `SessionEnd` | the host ends the session (`dispose_kernel`) or its engine drops | `reason` (`other`) | none |
  | `WorktreeCreate` | a managed worktree is made (`--worktree`, `rlm.enter_worktree`, child isolation); replaces git, so it also works outside a repository | `name` | exit 0 and the created directory as the last stdout line; anything else fails the creation. The first command runs |
  | `WorktreeRemove` | a worktree a `WorktreeCreate` hook made is removed | `worktree_path` | any non-zero exit fails the removal (the worktree is kept). Every command runs |

  `WorktreeCreate`/`WorktreeRemove` are not session events: `pa_hooks::worktree::install` registers them with
  `pa_core::worktree` (the composition root does), and they read the `hooks` settings of the directory the worktree
  is made from. `session_id` is empty for the `--worktree` launch (no session exists yet).

  `background_tasks` lists the kernel's `bash()` commands still running (tracked from `bash.run` until their result,
  a detached `bash.completed`, a `bash.kill` or `bash.killAll`) and the child agents still running.
- **Tool names hooks see:**
  - The model's own tool calls keep their names (`python_repl`, ...).
  - The kernel's `bash()` (`bash.run`) is `Bash` with `tool_input.command`.
  - The kernel's MCP calls (`mcp.session.call_tool`) are `mcp__<server>__<tool>`.
- **Matchers** are anchored regular expressions over the event's subject: the tool name (`PreToolUse`,
  `PostToolUse`), `source` (`SessionStart`), `agent_type` (`SubagentStart`, `SubagentStop`), `trigger`
  (`PreCompact`, `PostCompact`), `notification_type` (`Notification`), `reason` (`SessionEnd`). An empty or `*`
  matcher matches everything; `UserPromptSubmit`, `PostToolBatch` and `Stop` ignore matchers.
- **Answers:**
  - Exit 2 refuses (for `Stop`: keeps the agent going), with stderr as the reason.
  - JSON on stdout refuses with `permissionDecision: "deny"`, with `decision: "block"`, or with `continue: false`
    (for `Stop`/`SubagentStop`, `continue: false` lets the agent stop: it was stopping).
    `permissionDecision: "ask"` asks the user (`pa_core::features::confirm`); without a client that can ask, or
    without an answer, the action is refused (a policy must not fail open).
  - Additional context comes from `hookSpecificOutput.additionalContext`, or from plain stdout for `SessionStart`,
    `UserPromptSubmit` and `PreCompact`.
  - Any other exit, a timeout or a spawn failure is a reported, non-blocking error.
- **`Stop` continuations:** the reasons of every blocking `Stop` hook reach the model as one hidden row
  (`Stop hook feedback:\n<reasons>`, custom type `feature_continuation`) and the same run makes another model call.
  The next `Stop` input has `stop_hook_active: true`; a hook should let the agent stop then. The session also caps
  continuations at `MAX_FEATURE_CONTINUATIONS_PER_RUN` (8) per run.
- **Background hooks:** `"async": true` runs a command without the session waiting for it; its decisions are not
  applied and its context rides the next admitted prompt. `"asyncRewake": true` (implies `async`): exit 2 delivers
  `Async <Event> hook reported:\n\n<stderr>` to the model as a visible `feature_notice` row
  (`pa_core::features::deliver_feature_notice`), which starts a turn on an idle session or steers into a running
  one.
- **Process:** `sh -c` (`cmd /C` on Windows) in the session's cwd, with the host environment plus
  `CLAUDE_PROJECT_DIR`, `PRIME_AGENT_PROJECT_DIR` and `PRIME_AGENT_SESSION_ID`. The timeout is in seconds (default
  60). Output is capped at 64 KiB per stream. The matching commands of an event run in parallel, each once.
  `SessionEnd` runs on a thread of its own (the session may end outside any async runtime); `flush` waits for it
  before the process exits.

## Non-goals and not wired

- Claude Code's `prompt` and `agent` hooks, and events with no Prime Agent counterpart (`PermissionRequest`,
  `StopFailure`, `TeammateIdle`, `TaskCompleted`, ...). Unknown event keys are reported as unsupported and skipped.
- `SubagentStop` decisions: a `block` cannot keep a child going from the parent (the child runs in its own session).
  `SubagentStart` context cannot reach the child either. Both fire and forget.
- `SubagentStop` for a child that replied explicitly (`agent_message`) and is never collected or listed: a reply is
  not the end of a child, so only a settled `rlm.collect` result, a roster row, or a terminal notice ends one.
- `agent_transcript_path`: the child's transcript belongs to its own session; `agent_session_dir` carries the
  child's session directory when the spawn handle or a collect result reported it.
- `transcript_path`: always empty, because the conversation log belongs to the daemon.
- `systemMessage` and `suppressOutput`: there is no user-facing hook output surface; they are ignored.
- `SessionEnd` `reason` is always `other`: the engine does not know why its host ended it (`clear`, `logout`, ...).
- `Notification` for idle prompts (`idle_prompt`): the session has no idle timer.
- Separate `stdout`/`stderr` in a `Bash` `tool_response`: the kernel's commands write one merged stream.

## Public API

- `HooksFeature`: the `SessionFeature` `pa-cli` installs (feature `hooks`).
- `config::{HooksConfig, HookEvent, HookCommand, HookMode, DEFAULT_TIMEOUT}`.
- `run::{run, answer, HookAnswer, Outcome}`.
- `kernel_bash::{KernelJobs, RunningJob, FinishedCommand}`: a session's running kernel commands, for `PostToolUse`
  and `Stop`'s `background_tasks`.
- `children::{Children, Child, SettledChild}`: a session's child agents, for `SubagentStart`/`SubagentStop`.
- `host_request_as_tool` and `FEATURE_NAME`.

## Seams it plugs into

`pa_core::features::SessionFeature`: `on_session_start`, `prompt_context`, `review_tool_call`,
`review_host_request`, `after_host_request`, `after_tool_call`, `on_message_end`, `before_agent_stop`,
`after_tool_batch`, `before_compaction`, `after_compaction`, `on_session_end` and `flush`; and
`pa_core::features::deliver_feature_notice` for `asyncRewake`.

## Files it owns

None. It reads settings.

## Telemetry

None.

## Tests

- `src/config.rs`, `src/run.rs`, `src/kernel_bash.rs`, `src/children.rs`, `src/session.rs` and `src/tests.rs` cover
  matchers per event and skipped entries, every answer shape (a `Stop` block, `continue: false`, `PreCompact`
  stdout), a real command's stdin and timeout, job tracking across `bash.run`/`bash.follow`/`bash.completed`/
  `bash.kill` (spilled output, the cap), child tracking (start once, stop once from any source), the host-request
  tool mapping, and the seams over real shell hooks: `SubagentStart`/`SubagentStop`, `Notification` on a
  completion row, `asyncRewake` delivering a notice and an `async` hook's context riding the next prompt, `Stop`
  with `background_tasks`, `PostToolBatch`, `PreCompact`/`PostCompact`, and `SessionEnd` outside a runtime.
- `tests/session.rs` uses POSIX shell hooks in real sessions:
  - start and submit context;
  - a refused prompt that leaves the start context for the next one;
  - a `PreToolUse` refusal and a `PostToolUse` note;
  - `Stop`'s input;
  - a blocking `Stop` hook continuing the run once (the model reads its reason; the second stop carries
    `stop_hook_active`), with a `PostToolBatch` context riding the request after the tool batch;
  - an `asyncRewake` hook waking the idle session (a second turn runs on its notice), and `SessionEnd` when the
    host ends the session;
  - over a real kernel, a `Bash` hook denying `rm -rf` through `permissionDecision`, asking about two commands (one
    allowed, one refused through the confirmation surface) while `echo` runs, and a `PostToolUse` `Bash` hook whose
    feedback reaches the cell's result.
