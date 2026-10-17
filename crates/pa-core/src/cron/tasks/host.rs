//! The kernel host requests (Claude Code's `CronCreate`, `CronList`,
//! `CronDelete` and `ScheduleWakeup` tools): `rlm.cron.create`,
//! `rlm.cron.list`, `rlm.cron.delete`, `rlm.schedule_wakeup`. Registered
//! only when the embedding owns a scheduler (the daemon worker).

use std::sync::Arc;

use serde_json::{Value, json};

use super::book::{NewTask, TaskOrigin, TaskView, WakeupOutcome};
use super::expr::local_clock;
use super::session::SessionTasks;
use crate::kernel::shared::{HostRequestHandlers, host_handler};

fn optional_bool(data: &Value, key: &str, request: &str) -> anyhow::Result<Option<bool>> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => anyhow::bail!("{request} {key} must be a boolean"),
    }
}

fn optional_string<'a>(
    data: &'a Value,
    key: &str,
    request: &str,
) -> anyhow::Result<Option<&'a str>> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => anyhow::bail!("{request} {key} must be a string"),
    }
}

fn view_json(view: &TaskView) -> Value {
    json!({
        "id": view.id,
        "cron": view.cron,
        "human_schedule": view.human_schedule,
        "prompt": view.prompt,
        "kind": view.kind,
        "recurring": view.recurring,
        "durable": view.durable,
        "created_at": view.created_at,
        "last_fired_at": view.last_fired_at,
        "next_fire_at": view.next_fire_at,
        "reason": view.reason,
    })
}

/// `rlm.cron.create`.
///
/// # Errors
///
/// A malformed payload or the book's refusal.
pub fn cron_create(tasks: &SessionTasks, data: &Value) -> anyhow::Result<Value> {
    const REQUEST: &str = "rlm.cron.create";
    let cron = optional_string(data, "cron", REQUEST)?
        .ok_or_else(|| anyhow::anyhow!("{REQUEST} cron must be a string"))?;
    let prompt = optional_string(data, "prompt", REQUEST)?
        .ok_or_else(|| anyhow::anyhow!("{REQUEST} prompt must be a string"))?;
    let recurring = optional_bool(data, "recurring", REQUEST)?.unwrap_or(true);
    let durable = optional_bool(data, "durable", REQUEST)?.unwrap_or(false);
    let created = tasks
        .create(&NewTask {
            cron,
            prompt,
            recurring,
            durable,
            origin: TaskOrigin::Model,
        })
        .map_err(anyhow::Error::msg)?;
    Ok(json!({
        "id": created.id,
        "human_schedule": created.human_schedule,
        "recurring": created.recurring,
        "durable": created.durable,
        "next_fire_at": created.next_fire_at,
        "text": created.text,
    }))
}

/// `rlm.cron.list`.
///
/// # Errors
///
/// Scheduling is disabled.
pub fn cron_list(tasks: &SessionTasks) -> anyhow::Result<Value> {
    tasks.ensure_enabled().map_err(anyhow::Error::msg)?;
    let jobs: Vec<Value> = tasks.list().iter().map(view_json).collect();
    Ok(json!({ "jobs": jobs }))
}

/// `rlm.cron.delete`.
///
/// # Errors
///
/// No task carries the id.
pub fn cron_delete(tasks: &SessionTasks, data: &Value) -> anyhow::Result<Value> {
    const REQUEST: &str = "rlm.cron.delete";
    tasks.ensure_enabled().map_err(anyhow::Error::msg)?;
    let id = optional_string(data, "id", REQUEST)?
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{REQUEST} id must be a non-empty string"))?;
    match tasks.delete(id) {
        Some(task) => Ok(json!({
            "id": task.id,
            "deleted": true,
            "text": format!("Cancelled scheduled job {}.", task.id),
        })),
        None => anyhow::bail!("No scheduled job with id '{id}'."),
    }
}

/// `rlm.schedule_wakeup`.
///
/// # Errors
///
/// Claude Code's input errors: the delay and reason, and the prompt, are
/// required unless stopping.
pub fn schedule_wakeup(tasks: &SessionTasks, data: &Value) -> anyhow::Result<Value> {
    const REQUEST: &str = "rlm.schedule_wakeup";
    tasks.ensure_enabled().map_err(anyhow::Error::msg)?;
    if optional_bool(data, "stop", REQUEST)? == Some(true) {
        let cancelled = tasks.stop_dynamic_loop();
        let tail = "If you armed a monitor for this loop, stop it now (rlm.monitor_stop). Then write the loop's outcome for the user as ordinary visible response text, and end the turn.";
        let text = if cancelled == 0 {
            format!(
                "Loop stopped: any self-paced loop in this session is ended; there was no pending wakeup to cancel. A fixed-interval /loop (a recurring job) is NOT stopped by this call: cancel it with rlm.cron.delete or /loop stop. {tail}"
            )
        } else {
            format!(
                "Loop stopped: cancelled {cancelled} pending wakeup(s); no further self-paced wakeups are scheduled. {tail}"
            )
        };
        return Ok(json!({
            "stopped": true,
            "cancelled_wakeups": cancelled,
            "text": text,
        }));
    }
    let delay = match data.get("delay_seconds") {
        Some(Value::Number(number)) => number.as_f64(),
        None | Some(Value::Null) => None,
        Some(_) => anyhow::bail!("{REQUEST} delay_seconds must be a number"),
    };
    let reason = optional_string(data, "reason", REQUEST)?;
    let (Some(delay), Some(reason)) = (delay, reason) else {
        anyhow::bail!("`delay_seconds` and `reason` are required when `stop` is not true.");
    };
    let Some(prompt) = optional_string(data, "prompt", REQUEST)?.filter(|p| !p.trim().is_empty())
    else {
        anyhow::bail!("`prompt` is required when `stop` is not true.");
    };
    let noop = optional_bool(data, "noop", REQUEST)?.unwrap_or(false);
    match tasks.schedule_wakeup(delay, prompt, Some(reason), noop) {
        WakeupOutcome::Scheduled {
            scheduled_for,
            clamped_seconds,
            was_clamped,
        } => {
            let clock = local_clock(scheduled_for, tasks.zone());
            let in_seconds = ((scheduled_for - tasks.now()) / 1000).max(0);
            let clamp_note = if was_clamped {
                format!(" (clamped to {clamped_seconds}s from your requested value)")
            } else {
                String::new()
            };
            Ok(json!({
                "scheduled_for": scheduled_for,
                "clamped_delay_seconds": clamped_seconds,
                "was_clamped": was_clamped,
                "text": format!(
                    "Next wakeup scheduled for {clock} (in {in_seconds}s){clamp_note}. If you owe the user a status update this tick, write it now as ordinary response text; then end the turn: the host wakes you when the wakeup fires or a notice arrives."
                ),
            }))
        }
        WakeupOutcome::AgedOut => Ok(json!({
            "scheduled_for": 0,
            "clamped_delay_seconds": 0,
            "was_clamped": false,
            "text": "Wakeup not scheduled. The loop reached its maximum duration: the loop has ended; do not re-issue. Then write the loop's outcome for the user as ordinary visible response text, and end the turn.",
        })),
    }
}

/// Register the four handlers over the session's book.
pub fn register_host_handlers(handlers: &mut HostRequestHandlers, tasks: &Arc<SessionTasks>) {
    let create = Arc::clone(tasks);
    handlers.register(
        "rlm.cron.create",
        host_handler(move |payload| {
            let tasks = Arc::clone(&create);
            Box::pin(async move { cron_create(&tasks, &payload.data) })
        }),
    );
    let list = Arc::clone(tasks);
    handlers.register(
        "rlm.cron.list",
        host_handler(move |_payload| {
            let tasks = Arc::clone(&list);
            Box::pin(async move { cron_list(&tasks) })
        }),
    );
    let delete = Arc::clone(tasks);
    handlers.register(
        "rlm.cron.delete",
        host_handler(move |payload| {
            let tasks = Arc::clone(&delete);
            Box::pin(async move { cron_delete(&tasks, &payload.data) })
        }),
    );
    let wakeup = Arc::clone(tasks);
    handlers.register(
        "rlm.schedule_wakeup",
        host_handler(move |payload| {
            let tasks = Arc::clone(&wakeup);
            Box::pin(async move { schedule_wakeup(&tasks, &payload.data) })
        }),
    );
}
