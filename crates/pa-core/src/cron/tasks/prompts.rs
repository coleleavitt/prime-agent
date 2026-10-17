//! The text the scheduler hands the model: Claude Code's `/loop` skill
//! bodies, its autonomous-loop preambles and per-tick reminders, the
//! missed-task notice and the tool results — reworded for the kernel
//! surface (`rlm.cron.*`, `rlm.schedule_wakeup`, `rlm.monitor`).

/// The sentinel a fixed-interval autonomous `/loop` schedules as its prompt;
/// it expands at fire time.
pub const AUTONOMOUS_LOOP_SENTINEL: &str = "<<autonomous-loop>>";
/// The self-paced variant's sentinel (the one `rlm.schedule_wakeup` takes).
pub const AUTONOMOUS_LOOP_DYNAMIC_SENTINEL: &str = "<<autonomous-loop-dynamic>>";

/// Claude Code's autonomous-loop preamble.
pub const AUTONOMOUS_PREAMBLE: &str = include_str!("loop_autonomous_preamble.md");
/// The persistent variant (`PRIME_AGENT_LOOP_PERSISTENT`; Claude Code's
/// `CLAUDE_CODE_LOOP_PERSISTENT`).
pub const AUTONOMOUS_PREAMBLE_PERSISTENT: &str =
    include_str!("loop_autonomous_preamble_persistent.md");

/// Whether a prompt is one of the autonomous-loop sentinels.
#[must_use]
pub fn is_autonomous_sentinel(prompt: &str) -> bool {
    prompt == AUTONOMOUS_LOOP_SENTINEL || prompt == AUTONOMOUS_LOOP_DYNAMIC_SENTINEL
}

/// The preamble for this process (persistent when the env flag is set).
#[must_use]
pub fn autonomous_preamble(persistent: bool) -> &'static str {
    if persistent {
        AUTONOMOUS_PREAMBLE_PERSISTENT
    } else {
        AUTONOMOUS_PREAMBLE
    }
}

/// How a self-paced tick reports and re-arms (Claude Code's per-tick
/// guidance for a model that ends the turn after its last tool call).
const STATUS_UPDATE: &str = "This must be ordinary visible response text: the user cannot see your thinking, so an update written only there is invisible to them. Make it the last thing in the turn, then end the turn.";

fn dynamic_rearm_guidance() -> String {
    format!(
        "\n\nIf a monitor is armed (check `rlm.monitor_list()`), keep `delay_seconds` at 1200-1800: the monitor is the wake signal and this is only the fallback heartbeat. If you were woken by a notice (a `[monitor ...]` batch, a `[bash-done]` completion, a task notification), handle the event before deciding whether to re-arm. After re-arming, write a brief status update for this tick. {STATUS_UPDATE} To stop the loop, call `await rlm.schedule_wakeup(stop=True)` and `rlm.monitor_stop(id)` any monitor you armed."
    )
}

/// The short reminder a fixed-interval autonomous tick gets once the
/// preamble is in the conversation.
#[must_use]
pub fn autonomous_cron_tick() -> String {
    "# Autonomous loop tick\n\nRun the autonomous check using the loop instructions established earlier in this conversation. If you cannot find them, treat this as a no-op tick. The recurring schedule fires the next tick automatically: do not call `rlm.schedule_wakeup` from this tick.".to_string()
}

/// The short reminder a self-paced autonomous tick gets.
#[must_use]
pub fn autonomous_dynamic_tick() -> String {
    format!(
        "# Autonomous loop tick (dynamic pacing)\n\nRun the autonomous check using the loop instructions established earlier in this conversation. If you cannot find them, treat this as a no-op tick.\n\nYou scheduled this tick with `rlm.schedule_wakeup` (not a recurring schedule). To keep the loop alive, call `await rlm.schedule_wakeup(delay_seconds, prompt=\"{AUTONOMOUS_LOOP_DYNAMIC_SENTINEL}\", reason=..., noop=...)` again this turn, with `noop=True` if this tick changed nothing (or `noop=False` if it did); otherwise the loop ends after this tick.{}",
        dynamic_rearm_guidance()
    )
}

/// A fired autonomous sentinel: the full preamble on first delivery, the
/// short reminder after (the long instructions stay in the conversation).
#[must_use]
pub fn resolve_autonomous_fire(prompt: &str, preamble_delivered: bool, persistent: bool) -> String {
    let tick = if prompt == AUTONOMOUS_LOOP_DYNAMIC_SENTINEL {
        autonomous_dynamic_tick()
    } else {
        autonomous_cron_tick()
    };
    if preamble_delivered {
        return tick;
    }
    format!(
        "{}\n\n---\n\n{tick}",
        autonomous_preamble(persistent).trim_end()
    )
}

/// How to pick `delay_seconds` (Claude Code's cache-aware guidance for a
/// session whose prompt-cache lifetime is not known up front).
const PICKING_DELAY: &str = "## Picking delay_seconds\n\nThe prompt cache decides how expensive a wake-up is: waking inside the cache lifetime re-reads the conversation cached (fast, cheap); waking past it re-reads everything uncached. Never schedule extra wakeups just to keep the cache warm: they cost more than the miss they avoid. Match the delay to what you are actually waiting for: when polling external state the harness cannot notify you about (a CI run, a deploy, a remote queue), pick the delay from how fast that state changes; for idle ticks with nothing specific to watch, default to 1200-1800 s (20-30 min). On a five-minute cache, prefer 270 s over 300 s when actively polling, and commit to 1200 s or more rather than repeated ~300 s waits. The host clamps the delay to [60, 3600].";

/// The self-paced `/loop` body for a user prompt (Claude Code's dynamic
/// mode, run on every tick: the next tick's prompt is `/loop <input>`).
#[must_use]
pub fn dynamic_loop_instructions(input: &str) -> String {
    format!(
        "# /loop: self-paced\n\nThe user wants this task repeated, and you pick the pace. Decide what makes the next iteration worth running: a passage of time, or an observable event.\n\n1. **Run the task now.** Act on the input below directly.\n2. **If the next run is gated on an event** (CI finishing, a log line matching, a file changing) and no monitor is already running for it, arm one with `rlm.monitor(...)`. Its batches arrive as notices and wake this loop at once: you do not wait for the wakeup deadline. Arm once; on later iterations check `rlm.monitor_list()` first and skip this step if a monitor is already running.\n3. **Decide whether the loop continues.** If the task needs another iteration, call `await rlm.schedule_wakeup(delay_seconds, prompt=\"/loop {input}\", reason=\"...\", noop=...)`:\n   - `delay_seconds`: with a monitor armed this is the fallback heartbeat (lean 1200-1800 s); without one it is the cadence, picked from what you observed.\n   - `prompt`: the full /loop input verbatim, prefixed with `/loop `, so the next firing continues the loop.\n   - `reason`: one short, specific sentence on why you picked that delay (\"watching CI run\" beats \"waiting\"); the user sees it.\n   - `noop`: `True` if this iteration changed nothing (\"still waiting\"), `False` if it did something worth keeping.\n   If it does not need another iteration, stop instead (step 6): re-arming is a per-turn choice, not a default.\n4. **After the wakeup is armed, briefly confirm**: that you are self-pacing, whether a monitor is the wake signal, that you ran the task now, and the delay you picked. {STATUS_UPDATE}\n5. **If you were woken by a notice** rather than this prompt: handle the event in the context of the loop task, then make the same decision.\n6. **To stop the loop** (the task is complete, further iterations cannot make progress, or the user asked you to stop): call `await rlm.schedule_wakeup(stop=True)` and `rlm.monitor_stop(id)` any monitor you armed, then write the loop's outcome for the user as ordinary visible response text: a stopped loop has no next tick to surface it. The user can restart it any time with /loop.\n\n{PICKING_DELAY}\n\n## Input\n\n{input}"
    )
}

/// The self-paced autonomous default (`/loop` with no input).
#[must_use]
pub fn autonomous_dynamic_instructions(persistent: bool) -> String {
    format!(
        "# /loop: autonomous default with dynamic pacing\n\nThe user invoked /loop with no prompt and no interval. Run the autonomous check now, then pace the next iteration yourself with `rlm.schedule_wakeup`.\n\n## Action\n\n1. **Run the autonomous check now**, following the instructions below.\n2. **If the next tick is gated on an event** (CI finishing, a log line) and no monitor is already running for it, arm one with `rlm.monitor(...)`; its batches wake this loop at once.\n3. **Decide whether the loop continues.** If the next check is worth running, call `await rlm.schedule_wakeup(delay_seconds, prompt=\"{AUTONOMOUS_LOOP_DYNAMIC_SENTINEL}\", reason=\"...\", noop=...)`. Pass the literal sentinel as `prompt`: it expands at fire time to a short reminder of these instructions. Lean 1200-1800 s with a monitor armed; without one, pick from what you observed (a quiet branch waits longer).\n4. **After the wakeup is armed, briefly confirm**: that this is the autonomous default in dynamic-pacing mode, that you ran the check now, and the delay you picked. {STATUS_UPDATE}\n5. **To stop the loop**, call `await rlm.schedule_wakeup(stop=True)` and stop any monitor you armed, then write the loop's outcome as visible text.\n\n{PICKING_DELAY}\n\n## Autonomous-loop instructions (for this run and every tick)\n\n{}",
        autonomous_preamble(persistent).trim_end()
    )
}

/// The fixed-interval autonomous default (`/loop 10m`): the host already
/// scheduled the job; the model runs the first check now.
#[must_use]
pub fn autonomous_cron_instructions(scheduled: &str, persistent: bool) -> String {
    format!(
        "# /loop: autonomous default\n\nThe user invoked /loop with an interval and no prompt. {scheduled} Each tick fires the autonomous-loop sentinel, which expands to a short reminder of the instructions below.\n\n## Action\n\nRun the first autonomous check now, following the instructions below; do not wait for the first scheduled fire. Do not schedule anything yourself: the recurring job is the loop.\n\n## Autonomous-loop instructions (for this run and every tick)\n\n{}",
        autonomous_preamble(persistent).trim_end()
    )
}

/// The missed one-shot notice (Claude Code's text; the model asks in its
/// reply instead of an ask-the-user tool).
#[must_use]
pub fn missed_tasks_notice(entries: &[String]) -> String {
    let plural = entries.len() > 1;
    let head = if plural {
        "The following one-shot scheduled tasks were missed while this session was not running. They have already been removed from the session's scheduled_tasks.json.\n\nDo NOT execute these prompts yet. First ask the user whether to run each one now (end your turn with the question). Only execute if the user confirms."
    } else {
        "The following one-shot scheduled task was missed while this session was not running. It has already been removed from the session's scheduled_tasks.json.\n\nDo NOT execute this prompt yet. First ask the user whether to run it now (end your turn with the question). Only execute if the user confirms."
    };
    format!("{head}\n\n{}", entries.join("\n\n"))
}

/// `rlm.cron.create`'s result line.
#[must_use]
pub fn created_text(
    id: &str,
    human: &str,
    recurring: bool,
    durable: bool,
    max_days: i64,
) -> String {
    let storage = if durable {
        "Persisted to the session's scheduled_tasks.json (survives a daemon restart; resumes with the session)"
    } else {
        "Session-only (not written to disk, ends when this session closes)"
    };
    if recurring {
        format!(
            "Scheduled recurring job {id} ({human}). {storage}. Auto-expires after {max_days} days. Use rlm.cron.delete(\"{id}\") to cancel sooner."
        )
    } else {
        format!(
            "Scheduled one-shot task {id} ({human}). {storage}. It will fire once then auto-delete."
        )
    }
}
