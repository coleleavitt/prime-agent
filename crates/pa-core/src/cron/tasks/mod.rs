//! Claude Code's session scheduling ("kairos"): `CronCreate` / `CronList` /
//! `CronDelete`, `ScheduleWakeup` and `/loop`, as the kernel's `rlm.cron.*` and
//! `rlm.schedule_wakeup` plus the `/loop` session command.
//!
//! Separate from the TS-era [`super::store`] catalog (`prime-agent schedule`,
//! `/heartbeat`): these tasks keep Claude Code's semantics — five-field
//! cron in the user's local time, deterministic jitter, recurring tasks that
//! expire after seven days, one-shots that delete themselves, session-only
//! by default with opt-in durable tasks (the session's
//! `scheduled_tasks.json`, Claude Code's file shape) — and a fire only ever
//! reaches the session while it is idle.
//!
//! This module is pure plus one shared handle: [`book::TaskBook`] decides
//! everything against an injected clock and zone, [`session::SessionTasks`]
//! wraps it for the host-request handlers ([`host`]) and the embedding's
//! scheduler, which owns time and delivery (the daemon worker's
//! `session_tasks` module).

pub mod book;
pub mod expr;
pub mod host;
pub mod jitter;
pub mod loop_command;
pub mod prompts;
pub mod session;
pub mod store;

pub use book::{Fire, FireKind, TaskBook, TickSettle};
pub use session::{LoopRun, LoopTurn, SessionTasks, TaskFlags};

/// The injected row a scheduled task's fire starts its turn with.
pub const SCHEDULED_TASK_FIRE_CUSTOM_TYPE: &str = "scheduled_task_fire";
/// The injected row a `/loop` starts its first turn with.
pub const LOOP_PROMPT_CUSTOM_TYPE: &str = "loop_prompt";
