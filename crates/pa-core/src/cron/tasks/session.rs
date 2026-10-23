//! One session's shared task book: the kernel's `rlm.cron.*` and
//! `rlm.schedule_wakeup` host requests and the `/loop` command write it;
//! the embedding's scheduler (the daemon worker, which owns time and the
//! session's queue) reads what is due and delivers it while the session is
//! idle. Every mutation persists the durable tasks and wakes the scheduler.

use std::path::PathBuf;
use std::sync::Arc;

use pa_types::sync::MutexExt;

use super::book::{
    CreatedTask,
    Durability,
    Fire,
    NewTask,
    ScheduledTask,
    TaskBook,
    TaskOrigin,
    TaskView,
    TickSettle,
    WakeupOutcome,
};
use super::expr::{CronZone, SystemZone};
use super::jitter::JitterConfig;
use super::loop_command::{LoopInterval, LoopPlan, parse_loop};
use super::prompts::{
    AUTONOMOUS_LOOP_SENTINEL,
    autonomous_cron_instructions,
    autonomous_dynamic_instructions,
    dynamic_loop_instructions,
};
use super::store::{SCHEDULED_TASKS_FILENAME, read_durable_tasks, write_durable_tasks};

/// The scheduler's clock (epoch ms); tests inject theirs.
pub type TaskClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// `PRIME_AGENT_DISABLE_CRON`: the scheduling surface refuses (Claude
/// Code's `CLAUDE_CODE_DISABLE_CRON`).
pub const DISABLE_ENV: &str = "PRIME_AGENT_DISABLE_CRON";
/// `PRIME_AGENT_LOOP_PERSISTENT`: the autonomous loop's persistent preamble.
pub const PERSISTENT_ENV: &str = "PRIME_AGENT_LOOP_PERSISTENT";
/// `PRIME_AGENT_LOOP_KEEPALIVE=0`: no fallback wakeup when a self-paced tick
/// ends without re-arming (on by default, like Claude Code's).
pub const KEEPALIVE_ENV: &str = "PRIME_AGENT_LOOP_KEEPALIVE";

/// The flags a session's scheduler runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskFlags {
    pub disabled: bool,
    pub persistent_preamble: bool,
    pub keepalive: bool,
}

impl Default for TaskFlags {
    fn default() -> Self {
        Self {
            disabled: false,
            persistent_preamble: false,
            keepalive: true,
        }
    }
}

fn env_flag(name: &str) -> Option<bool> {
    let value = std::env::var(name).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "0" | "false" | "no" | "off" => Some(false),
        _ => Some(true),
    }
}

impl TaskFlags {
    /// The flags from this process's environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            disabled: env_flag(DISABLE_ENV).unwrap_or(false),
            persistent_preamble: env_flag(PERSISTENT_ENV).unwrap_or(false),
            keepalive: env_flag(KEEPALIVE_ENV).unwrap_or(true),
        }
    }
}

/// The message every scheduling call answers while disabled.
pub const DISABLED_MESSAGE: &str =
    "Scheduled tasks are disabled in this session (PRIME_AGENT_DISABLE_CRON is set).";

pub struct SessionTasks {
    book: std::sync::Mutex<TaskBook>,
    zone: Arc<dyn CronZone>,
    clock: TaskClock,
    flags: TaskFlags,
    durable_path: std::sync::Mutex<Option<PathBuf>>,
    bound_session: std::sync::Mutex<Option<String>>,
    changed: tokio::sync::Notify,
}

impl std::fmt::Debug for SessionTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTasks")
            .field("flags", &self.flags)
            .finish_non_exhaustive()
    }
}

impl SessionTasks {
    /// A book over `zone` and `clock`.
    #[must_use]
    pub fn new(
        book: TaskBook,
        zone: Arc<dyn CronZone>,
        clock: TaskClock,
        flags: TaskFlags,
    ) -> Arc<Self> {
        Arc::new(Self {
            book: std::sync::Mutex::new(book),
            zone,
            clock,
            flags,
            durable_path: std::sync::Mutex::new(None),
            bound_session: std::sync::Mutex::new(None),
            changed: tokio::sync::Notify::new(),
        })
    }

    /// The product book: the system zone and clock, the environment's flags.
    #[must_use]
    pub fn system() -> Arc<Self> {
        Self::new(
            TaskBook::new(JitterConfig::default()),
            Arc::new(SystemZone::system()),
            Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| {
                        i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
                    })
            }),
            TaskFlags::from_env(),
        )
    }

    #[must_use]
    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    #[must_use]
    pub fn zone(&self) -> &dyn CronZone {
        self.zone.as_ref()
    }

    #[must_use]
    pub fn flags(&self) -> TaskFlags {
        self.flags
    }

    /// Notified after every mutation: the scheduler re-reads the next fire.
    #[must_use]
    pub fn changed(&self) -> &tokio::sync::Notify {
        &self.changed
    }

    /// Fail while scheduling is disabled.
    ///
    /// # Errors
    ///
    /// [`DISABLED_MESSAGE`] when `PRIME_AGENT_DISABLE_CRON` is set.
    pub fn ensure_enabled(&self) -> Result<(), String> {
        if self.flags.disabled {
            Err(DISABLED_MESSAGE.to_string())
        } else {
            Ok(())
        }
    }

    /// Run `mutate` on the book at the current clock reading, then persist
    /// the durable tasks if they changed and wake the scheduler.
    pub fn with_book<R>(&self, mutate: impl FnOnce(&mut TaskBook, i64, &dyn CronZone) -> R) -> R {
        self.mutate(true, mutate)
    }

    /// [`Self::with_book`]; `wake: false` for the scheduler's own passes (a
    /// wake there would spin its loop).
    fn mutate<R>(
        &self,
        wake: bool,
        mutate: impl FnOnce(&mut TaskBook, i64, &dyn CronZone) -> R,
    ) -> R {
        let now = self.now();
        let (result, durable) = {
            let mut book = self.book.lock_or_recover();
            let result = mutate(&mut book, now, self.zone.as_ref());
            (result, book.take_durable_changes())
        };
        if let Some(tasks) = durable {
            let path = self.durable_path.lock_or_recover().clone();
            if let Some(path) = path {
                if let Err(error) = write_durable_tasks(&path, &tasks) {
                    tracing::warn!(%error, path = %path.display(), "scheduled tasks not saved");
                }
            }
        }
        if wake {
            self.changed.notify_one();
        }
        result
    }

    /// Bind the live session: a different session than the bound one
    /// starts from an empty book (session-only tasks end with their
    /// session), then loads the new session's durable tasks from its
    /// artifact partition. `artifacts` is `Err(reason)` when durable tasks
    /// cannot be kept (an in-memory session, a subagent). Returns the
    /// missed one-shot notice, when the resumed session missed any.
    pub fn bind(&self, session_id: &str, artifacts: Result<PathBuf, String>) -> Option<Fire> {
        {
            let mut bound = self.bound_session.lock_or_recover();
            if bound.as_deref() == Some(session_id) {
                return None;
            }
            *bound = Some(session_id.to_string());
        }
        let (path, durability) = match artifacts {
            Ok(dir) => (
                Some(dir.join(SCHEDULED_TASKS_FILENAME)),
                Durability::Available,
            ),
            Err(reason) => (None, Durability::Unavailable(reason)),
        };
        self.durable_path.lock_or_recover().clone_from(&path);
        let records = path.as_deref().map(read_durable_tasks).unwrap_or_default();
        self.with_book(move |book, now, zone| {
            book.clear();
            book.set_durability(durability);
            book.load_durable(records, now, zone)
        })
    }

    /// `rlm.cron.create` / a fixed `/loop`.
    ///
    /// # Errors
    ///
    /// The book's validation messages, or the disabled message.
    pub fn create(&self, request: &NewTask<'_>) -> Result<CreatedTask, String> {
        self.ensure_enabled()?;
        self.with_book(|book, now, zone| book.create(request, now, zone))
    }

    pub fn delete(&self, id: &str) -> Option<ScheduledTask> {
        self.with_book(|book, _, _| book.delete(id))
    }

    #[must_use]
    pub fn list(&self) -> Vec<TaskView> {
        let mut book = self.book.lock_or_recover();
        book.list(self.zone.as_ref())
    }

    pub fn schedule_wakeup(
        &self,
        delay_seconds: f64,
        prompt: &str,
        reason: Option<&str>,
        noop: bool,
    ) -> WakeupOutcome {
        self.with_book(|book, now, zone| {
            book.schedule_wakeup(delay_seconds, prompt, reason, noop, now, zone)
        })
    }

    pub fn stop_dynamic_loop(&self) -> usize {
        self.with_book(|book, _, _| book.stop_dynamic_loop())
    }

    pub fn stop_loops(&self) -> usize {
        self.with_book(|book, _, _| book.stop_loops())
    }

    pub fn user_abort(&self) -> usize {
        self.with_book(|book, _, _| book.user_abort())
    }

    /// Take what is due now. The caller has checked the session is idle.
    pub fn take_due(&self) -> Vec<Fire> {
        if self.flags.disabled {
            return Vec::new();
        }
        self.mutate(false, |book, now, zone| book.take_due(now, zone))
    }

    #[must_use]
    pub fn next_due_at(&self) -> Option<i64> {
        if self.flags.disabled {
            return None;
        }
        let mut book = self.book.lock_or_recover();
        book.next_due_at(self.zone.as_ref())
    }

    pub fn tick_settled(&self) -> TickSettle {
        let keepalive = self.flags.keepalive;
        self.with_book(|book, now, zone| book.tick_settled(now, keepalive, zone))
    }

    pub fn resolve_fire_prompt(&self, prompt: &str) -> String {
        let persistent = self.flags.persistent_preamble;
        self.book
            .lock_or_recover()
            .resolve_fire_prompt(prompt, persistent)
    }

    pub fn note_preamble_delivered(&self) {
        self.book.lock_or_recover().note_preamble_delivered();
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.book.lock_or_recover().has_pending()
    }

    #[must_use]
    pub fn max_age_days(&self) -> i64 {
        self.book.lock_or_recover().config().max_age_days()
    }

    /// Run `/loop <args>`: schedule what the host can (a fixed interval's
    /// recurring job, a stop) and hand back the result line plus the turn
    /// the model runs now.
    ///
    /// # Errors
    ///
    /// A usage error, the book's refusal, or the disabled message.
    pub fn run_loop_command(&self, args: &str) -> Result<LoopRun, String> {
        self.ensure_enabled()?;
        let persistent = self.flags.persistent_preamble;
        let days = self.max_age_days();
        match parse_loop(args)? {
            LoopPlan::Stop => {
                let cancelled = self.stop_loops();
                Ok(LoopRun {
                    text: if cancelled == 0 {
                        "No loop is running.".to_string()
                    } else {
                        format!("Loop stopped: cancelled {cancelled} scheduled loop task(s).")
                    },
                    turn: None,
                })
            }
            LoopPlan::Fixed { interval, prompt } => {
                let created = self.create(&NewTask {
                    cron: &interval.cron,
                    prompt: &prompt,
                    recurring: true,
                    durable: false,
                    origin: TaskOrigin::Loop,
                })?;
                Ok(LoopRun {
                    text: fixed_confirmation(&created, &interval, days),
                    turn: Some(LoopTurn {
                        mode: "fixed",
                        content: prompt,
                        job_id: Some(created.id),
                        schedule: Some(created.human_schedule),
                    }),
                })
            }
            LoopPlan::AutonomousFixed { interval } => {
                let created = self.create(&NewTask {
                    cron: &interval.cron,
                    prompt: AUTONOMOUS_LOOP_SENTINEL,
                    recurring: true,
                    durable: false,
                    origin: TaskOrigin::Loop,
                })?;
                let text = fixed_confirmation(&created, &interval, days);
                self.note_preamble_delivered();
                Ok(LoopRun {
                    turn: Some(LoopTurn {
                        mode: "autonomous",
                        content: autonomous_cron_instructions(&text, persistent),
                        job_id: Some(created.id),
                        schedule: Some(created.human_schedule),
                    }),
                    text: format!("{text} This is the autonomous default: each tick runs the autonomous-loop check."),
                })
            }
            LoopPlan::Dynamic { prompt } => Ok(LoopRun {
                text: "Self-paced loop: the model runs the task now and schedules each next run with rlm.schedule_wakeup. /loop stop ends it.".to_string(),
                turn: Some(LoopTurn {
                    mode: "dynamic",
                    content: dynamic_loop_instructions(&prompt),
                    job_id: None,
                    schedule: None,
                }),
            }),
            LoopPlan::AutonomousDynamic => {
                self.note_preamble_delivered();
                Ok(LoopRun {
                    text: "Self-paced autonomous loop: the model runs the autonomous check now and schedules each next tick with rlm.schedule_wakeup. /loop stop ends it.".to_string(),
                    turn: Some(LoopTurn {
                        mode: "autonomous",
                        content: autonomous_dynamic_instructions(persistent),
                        job_id: None,
                        schedule: None,
                    }),
                })
            }
        }
    }
}

/// What `/loop` did: the result line, and the turn the model runs now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopRun {
    pub text: String,
    pub turn: Option<LoopTurn>,
}

/// The turn a `/loop` starts (an injected `loop_prompt` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopTurn {
    /// `fixed`, `dynamic`, or `autonomous`.
    pub mode: &'static str,
    pub content: String,
    pub job_id: Option<String>,
    pub schedule: Option<String>,
}

fn fixed_confirmation(created: &CreatedTask, interval: &LoopInterval, days: i64) -> String {
    let rounded = interval
        .rounded_to
        .as_ref()
        .map(|to| {
            format!(
                " ({} does not divide evenly; rounded to {to})",
                interval.typed
            )
        })
        .unwrap_or_default();
    format!(
        "Scheduled recurring job {} ({}, cron `{}`){rounded}. It runs while this session is open, only when the session is idle, and auto-expires after {days} days. /loop stop or rlm.cron.delete(\"{}\") cancels it sooner.",
        created.id, created.human_schedule, interval.cron, created.id
    )
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
