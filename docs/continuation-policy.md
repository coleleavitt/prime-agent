# Continuation policy: consolidating the two self-continuation planes

Status: proposed (2026-11-13). Design note for a staged refactor.

## Problem

Prime Agent decides "keep going at a natural turn end" in **two** places that do
not share a decision, a cap, an injection path, or a config surface:

- **Plane A — the agent-loop natural-turn-end hook**
  (`agent.set_continuation_hook` -> `run_continuation`,
  `pa-agent/src/agent.rs`). Mints a continuation user row that runs as the next
  turn inside the same prompt wait. Installed twice: the daemon
  (`pa-daemon/src/autonomous_continuation.rs`, interactive autonomous: goal arm +
  gate arm + compaction) and headless
  (`pa-core/.../headless/continuation.rs`, print: goal + autonomous). Decides on
  **gates + limits + goal state**; never reads the assistant text.

- **Plane B — the feature `before_agent_stop` boundary**
  (`pa-core/.../feature_boundary.rs` -> `review_agent_stop` ->
  `steer_hidden_row`, capped at `MAX_FEATURE_CONTINUATIONS_PER_RUN`). Implemented
  by `pa-guards` (`GuardsFeature`, always installed). Decides on **assistant
  stall text** (`STALL_PHRASES` / `ACTION_LEADS`); never sees gate/goal state.

In a live interactive session both fire at the same boundary through different
mechanisms, with separate caps and configs (`/autonomous` +
`AgentAutonomousConfig` vs the `PA_GUARDS_CONTINUE_MODE` env var). With
autonomous off (the default) only Plane B runs, text-only, so a bare
"Want me to ...?" is left for the user and they type `continue` by hand.

## Evidence (CFG)

`continuation::decide` (Plane B, 27-block MIR CFG) and
`autonomous_continuation_rows` (Plane A cascade) are the **same control-flow
skeleton** with three swappable slots:

```
GATES      -> PRIORITY -> SUPPRESS -> SIGNAL(leaf) -> CAP -> SINK
```

| slot            | Plane B                         | Plane A                                   | consolidation |
|-----------------|---------------------------------|-------------------------------------------|---------------|
| GATES/PRIORITY  | plan_mode, auto_continue        | session_closed, input_queued, goal, compaction | union |
| SUPPRESS        | provider-error, waiting (text)  | unsettled-rlm, live-bg-bash (state)       | union |
| **SIGNAL leaf** | stall-text (block B12)          | gates / limits / threshold                | **pluggable** |
| CAP             | `MAX_FEATURE_CONTINUATIONS`     | `max_continuations`                       | one cap |
| SINK            | `steer_hidden_row` (boundary)   | in-run rows (hook)                        | keep the hook |

Block **B12** (`is_some_and(mode leaf)`) is the only line that varies by mode
today. Everything else is boilerplate re-implemented per plane.

## Design

One `ContinuationPolicy` with the shared spine and a pluggable signal leaf:

```
ContinuationPolicy::decide(ctx) -> Continue { reason } | Hold | HandOff | Stop
  1. GATES     (union of both gate sets)                         -> Stop
  2. PRIORITY  (goal / requested-compaction)                     -> HandOff
  3. SUPPRESS  (provider-err, waiting, unsettled-rlm, bg-bash)   -> Hold
  4. SIGNAL    = ctx.signal.stalled(ctx)   <- pluggable leaf (was B12)
  5. CAP       (one count vs one max)                            -> Stop
  6. SINK      one injection: the agent-loop continuation row
```

```rust
trait ContinuationSignal {
    // Continue { reason } | NoSignal | Uncertain
    fn stalled(&self, ctx: &ContinuationContext) -> Signal;
}
```

Leaf implementations, selected by one `continuation.mode` setting that replaces
both `/autonomous` (kept as a thin alias) and `PA_GUARDS_CONTINUE_MODE`:

- `off`        — never self-continue.
- `heuristic`  — the stall-text leaf (today's Plane B), de-slopped (default).
- `assisted`   — heuristic, and on `Uncertain` consult a small model via
  `session_engine::side_model` (`SideModel::Selector`). The model makes the
  semantic call a phrase list cannot: an action-shaped question
  ("Want me to add the note?" -> drive) vs a real branch choice
  ("1... 2... 3...?" -> stop).
- `autonomous` — the gate/limit leaf; drives through bare questions too.
- `peer`       — the `pa-observer` shape: a second model watches the turn
  stream asynchronously and messages the worker (folds in mid-turn) when it
  judges a stall. Off the boundary, so no paint-latency cost; continuous token
  spend, so opt-in.

A synchronous model call in the stop boundary is forbidden by the performance
rule (AGENTS.md -> "Performance and the critical path"); `goal_evaluator`
(the one existing model-at-stop-boundary) is off by default for exactly this
reason. `assisted` pays the round-trip only on the `Uncertain` fraction; `peer`
pays it async.

## The permutations the heuristic must handle

From the 2026-11-13 corpus of turns where the user typed `continue`:

| shape | example tail | leaf |
|---|---|---|
| bare permission question | "Want me to add the note?" | `assisted` (action-shaped -> drive) |
| branch-choice question | "1... 2... 3...?" | leave for the user |
| action-lead after a conjunction | "...didn't survive, so first I'll reread" | `heuristic` (missed anchor) |
| ended on a results table / data block | a trailing `|...|` row, no closing prose | `autonomous`/`assisted` |
| user-directed imperative handoff | "Restart the daemon to use it." | `assisted` (ambiguous for a regex) |
| remaining-work enumerated, not claimed | "X still needs..., Y still unverified" | `autonomous`/`assisted` |
| legit wait / provider error | "...running in the background" | suppressed (both) |

## Staged plan

1. **Heuristic precision (this stage).** Extend the stall heuristic for the
   high-confidence shapes: the conjunction action-lead (both modes, a missed
   anchor of an existing strong signal), and the data-block / remaining-work
   shapes (autonomous leaf, consistent with its "any stall" contract). Leave the
   genuinely ambiguous shapes (user-imperative, branch-choice) to the model leaf.
   Tests over the corpus tails. Contained to `pa-guards`.
2. **One spine, pluggable leaf.** Lift the shared spine into
   `ContinuationPolicy::decide`; make the signal a `ContinuationSignal` trait;
   the current stall-text becomes `HeuristicSignal`. The spine lives where it can
   see the full context (the agent-loop hook), not the impoverished
   `before_agent_stop` observation (which carries only text + `continued`).
3. **One injection path.** Route both planes through the agent-loop hook; retire
   `pa-guards`' `steer_hidden_row` injection (the `before_agent_stop` seam stays
   for `pa-hooks`' Stop hooks). One cap.
4. **One config.** A `continuation.mode` setting; `/autonomous` becomes an alias;
   remove `PA_GUARDS_CONTINUE_MODE`. Add the `assisted` (model) and `peer`
   (observer) leaves.
