# pa-guards

Quality guards for a Prime Agent session: a post-edit **slop guard**, a
task-completion **scaffold gate**, and a turn-end **self-continuation** decision.
Ported from the standalone `agent-guards` crate (extracted from jfc), whose
`FINDINGS.md` records the corpus measurements behind the thresholds here.

## Scope

- **Slop guard** (`slop_guard`): line-level checks over the source a cell wrote,
  scoped to the changed lines (complexity, duplication, swallowed errors, blocking
  calls in async, unsafe boundaries, test smells, …). Advice only — it never
  blocks. Measured to cut ~90% of whole-file noise by scoping to the diff.
- **Self-continuation** (`continuation`): when a turn ends, decide whether the
  model stalled (announced an action it did not take, teed up output with a
  trailing colon, or — in autonomous mode — asked a bare permission question) and
  return a reason that keeps it going. Suppresses provider errors and legitimate
  background waits. Two modes: `Interactive` (default, leaves a real question for
  the user) and `Autonomous` (`PA_GUARDS_CONTINUE_MODE=autonomous`).
- **Scaffold gate** (`scaffold_detector` + `work_quality`): scan the added lines
  of a change for stub patterns, weighted by severity, and block completion over a
  threshold. Pure logic, **not wired** (see non-goals).

## Non-goals

- The scaffold gate is not wired into the session: Prime Agent's task state lives
  in a Python skill, not a native tool, so there is no tool-call seam to gate yet.
  The logic is available for a future seam or an explicit command.
- No whole-repo audit from inside the session; use the `agent-guards sweep` CLI.

## Public API

- `GuardsFeature` — the `pa_core::features::SessionFeature` the composition root
  installs.
- `slop_guard::{run_all_checks_with_old, run_all_checks_scoped, SlopScope, format_report}`
- `continuation::{decide, assistant_text_stalls, assistant_announces_action, ContinueMode, ContinueReason, TurnEnd}`
- `scaffold_detector`, `work_quality::evaluate_work_quality`

## Seams

- `SessionFeature::after_tool_call` — reads the cell result's `fileWrites` host
  fact, runs the scoped slop guard over each source file (old = `git show HEAD:…`),
  and appends the findings to the result the model sees.
- `SessionFeature::before_agent_stop` — runs the continuation decision over
  `last_assistant_message`; `Some(reason)` keeps the agent going.

Installed only when `pa-cli` is built with `--features guards` (on by default).

## Files

`src/{lib,feature,slop_guard,scaffold_detector,work_quality,continuation,rust_lex}.rs`.

## Telemetry

None yet. Findings reach the model inline via the tool result and the stop reason.

## Environment

- `PA_GUARDS_CONTINUE_MODE=autonomous` — drive through bare questions too.
- `PA_GUARDS_CONTINUE_REPEAT=1` — allow a second continuation in one run.
