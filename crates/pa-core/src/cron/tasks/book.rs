//! The session's task book: every scheduled task and loop wakeup, when each
//! fires next, and the self-paced loop's state. Pure: every operation takes
//! the clock reading and the zone, so the schedule's decisions are tested
//! without a timer.

use std::collections::HashMap;

use super::expr::{CronZone, human_schedule, local_stamp, next_fire, parse};
use super::jitter::{JitterConfig, aged_out, one_shot_fire_at, recurring_fire_at};
use super::prompts;
use super::store::DurableTask;

/// Claude Code's cap on scheduled jobs per session.
pub const MAX_TASKS: usize = 50;
/// `rlm.schedule_wakeup`'s clamp, in seconds.
pub const MIN_WAKEUP_SECONDS: i64 = 60;
pub const MAX_WAKEUP_SECONDS: i64 = 3600;
/// The fallback wakeup the host arms when a self-paced tick ends without
/// re-arming (Claude Code's keepalive).
pub const KEEPALIVE_DELAY_SECONDS: f64 = 1200.0;
/// Keepalives in a row before the loop ends (the model declined twice).
const KEEPALIVE_BUDGET: u32 = 1;
/// A loop chain whose last wakeup is this far past is a new chain.
const CHAIN_STALE_MS: i64 = MAX_WAKEUP_SECONDS * 1000;

/// Who made a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOrigin {
    /// `rlm.cron.create`.
    Model,
    /// A fixed-interval `/loop` (`/loop stop` cancels it).
    Loop,
}

/// A recurring or one-shot cron task, or a self-paced loop's wakeup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskKind {
    Cron {
        recurring: bool,
        durable: bool,
        origin: TaskOrigin,
    },
    Wakeup {
        scheduled_for: i64,
        reason: Option<String>,
        /// Armed by the host, not the model.
        keepalive: bool,
        /// The model said its tick changed nothing.
        noop: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledTask {
    pub id: String,
    /// The expression (a wakeup's is its fire minute, for display).
    pub cron: String,
    pub prompt: String,
    pub created_at: i64,
    pub last_fired_at: Option<i64>,
    pub kind: TaskKind,
}

impl ScheduledTask {
    fn is_wakeup(&self) -> bool {
        matches!(self.kind, TaskKind::Wakeup { .. })
    }

    fn is_durable(&self) -> bool {
        matches!(self.kind, TaskKind::Cron { durable: true, .. })
    }

    fn is_recurring(&self) -> bool {
        matches!(
            self.kind,
            TaskKind::Cron {
                recurring: true,
                ..
            }
        )
    }

    fn started_by_loop(&self) -> bool {
        matches!(
            self.kind,
            TaskKind::Cron {
                origin: TaskOrigin::Loop,
                ..
            } | TaskKind::Wakeup { .. }
        )
    }
}

/// One listed task (`rlm.cron.list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView {
    pub id: String,
    pub cron: String,
    pub human_schedule: String,
    pub prompt: String,
    /// `cron`, `loop` (a fixed `/loop`), or `wakeup`.
    pub kind: &'static str,
    pub recurring: bool,
    pub durable: bool,
    pub created_at: i64,
    pub last_fired_at: Option<i64>,
    pub next_fire_at: Option<i64>,
    pub reason: Option<String>,
}

/// A cron task to schedule.
#[derive(Debug, Clone, Copy)]
pub struct NewTask<'a> {
    pub cron: &'a str,
    pub prompt: &'a str,
    pub recurring: bool,
    pub durable: bool,
    pub origin: TaskOrigin,
}

/// A created task (`rlm.cron.create`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedTask {
    pub id: String,
    pub human_schedule: String,
    pub recurring: bool,
    pub durable: bool,
    pub next_fire_at: Option<i64>,
    pub text: String,
}

/// One task that is due: what the host delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fire {
    pub task_id: String,
    /// The task's prompt as scheduled (a sentinel or `/loop <input>`
    /// included); [`TaskBook::resolve_fire_prompt`] expands it.
    pub prompt: String,
    pub cron: String,
    pub kind: FireKind,
    /// The transcript header (`Running scheduled task (Oct 17 2:30pm)`).
    pub header: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireKind {
    Cron {
        recurring: bool,
        final_fire: bool,
    },
    /// A self-paced loop tick; `noop_streak` counts the quiet ticks before
    /// it.
    Wakeup {
        noop_streak: u32,
    },
    /// The missed one-shot notice a resumed session gets.
    Missed,
}

/// What `rlm.schedule_wakeup` did.
#[derive(Debug, Clone, PartialEq)]
pub enum WakeupOutcome {
    Scheduled {
        scheduled_for: i64,
        clamped_seconds: i64,
        was_clamped: bool,
    },
    /// The loop reached the age limit: it ended, nothing was scheduled.
    AgedOut,
}

/// What the host does after a delivered loop tick settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickSettle {
    /// No loop tick was in flight.
    NoTick,
    /// The model re-armed during the tick.
    Rearmed,
    /// The model did not re-arm; the host armed the fallback wakeup.
    KeepaliveArmed { scheduled_for: i64 },
    /// The loop ended (the model stopped re-arming, or keepalive is off).
    Ended,
}

#[derive(Debug, Clone, Copy)]
struct Chain {
    started_at: i64,
    last_scheduled_for: i64,
}

#[derive(Debug, Default)]
struct LoopState {
    /// Per loop prompt: when the chain of wakeups started.
    chains: HashMap<String, Chain>,
    /// The prompt of the delivered loop tick whose turn is running.
    tick_in_flight: Option<String>,
    keepalives_in_a_row: u32,
    noop_streak: u32,
    streak_since: Option<i64>,
    /// The autonomous preamble is in the conversation.
    preamble_delivered: bool,
}

/// Where durable tasks go, when anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Durability {
    Available,
    /// Why `durable=True` is refused here.
    Unavailable(String),
}

/// The session's tasks.
pub struct TaskBook {
    config: JitterConfig,
    tasks: Vec<ScheduledTask>,
    /// Each task's computed next fire (`i64::MAX`: never).
    next_fire: HashMap<String, i64>,
    durability: Durability,
    durable_dirty: bool,
    loops: LoopState,
    new_id: Box<dyn FnMut() -> String + Send>,
}

impl std::fmt::Debug for TaskBook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskBook")
            .field("tasks", &self.tasks)
            .field("durability", &self.durability)
            .finish_non_exhaustive()
    }
}

/// Eight random hex digits (Claude Code's id shape; the jitter keys on it).
#[must_use]
pub fn random_task_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

impl Default for TaskBook {
    fn default() -> Self {
        Self::new(JitterConfig::default())
    }
}

impl TaskBook {
    #[must_use]
    pub fn new(config: JitterConfig) -> Self {
        Self {
            config,
            tasks: Vec::new(),
            next_fire: HashMap::new(),
            durability: Durability::Unavailable("durable tasks need a saved session".to_string()),
            durable_dirty: false,
            loops: LoopState::default(),
            new_id: Box::new(random_task_id),
        }
    }

    /// Replace the id source (tests pin ids; the jitter keys on them).
    pub fn set_id_source(&mut self, source: Box<dyn FnMut() -> String + Send>) {
        self.new_id = source;
    }

    #[must_use]
    pub fn config(&self) -> &JitterConfig {
        &self.config
    }

    pub fn set_durability(&mut self, durability: Durability) {
        self.durability = durability;
    }

    /// Whether any task or wakeup is pending (it keeps the session resident).
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.tasks.is_empty()
    }

    #[must_use]
    pub fn has_pending_wakeup(&self) -> bool {
        self.tasks.iter().any(ScheduledTask::is_wakeup)
    }

    /// Forget every task and the loop state (a session replacement).
    pub fn clear(&mut self) {
        self.tasks.clear();
        self.next_fire.clear();
        self.loops = LoopState::default();
        self.durable_dirty = false;
    }

    fn mint_id(&mut self) -> String {
        loop {
            let id = (self.new_id)();
            if !self.tasks.iter().any(|task| task.id == id) {
                return id;
            }
        }
    }

    /// Schedule a cron task (`rlm.cron.create`, a fixed `/loop`).
    ///
    /// # Errors
    ///
    /// Claude Code's validation messages: an invalid expression, one that
    /// never matches, the 50-task cap, or durability unavailable here.
    pub fn create(
        &mut self,
        request: &NewTask<'_>,
        now: i64,
        zone: &dyn CronZone,
    ) -> Result<CreatedTask, String> {
        let NewTask {
            cron,
            prompt,
            recurring,
            durable,
            origin,
        } = *request;
        let cron = cron.split_whitespace().collect::<Vec<_>>().join(" ");
        if parse(&cron).is_none() {
            return Err(format!(
                "Invalid cron expression '{cron}'. Expected 5 fields: M H DoM Mon DoW."
            ));
        }
        if next_fire(&cron, now, zone).is_none() {
            return Err(format!(
                "Cron expression '{cron}' does not match any calendar date."
            ));
        }
        if prompt.trim().is_empty() {
            return Err("prompt must be a non-empty string".to_string());
        }
        if self.tasks.len() >= MAX_TASKS {
            return Err(format!(
                "Too many scheduled jobs (max {MAX_TASKS}). Cancel one first."
            ));
        }
        if durable {
            if let Durability::Unavailable(reason) = &self.durability {
                return Err(reason.clone());
            }
        }
        let id = self.mint_id();
        let task = ScheduledTask {
            id: id.clone(),
            cron: cron.clone(),
            prompt: prompt.to_string(),
            created_at: now,
            last_fired_at: None,
            kind: TaskKind::Cron {
                recurring,
                durable,
                origin,
            },
        };
        let next_fire_at = self.schedule(&task, zone);
        self.tasks.push(task);
        if durable {
            self.durable_dirty = true;
        }
        let human = human_schedule(&cron);
        Ok(CreatedTask {
            text: prompts::created_text(
                &id,
                &human,
                recurring,
                durable,
                self.config.max_age_days(),
            ),
            id,
            human_schedule: human,
            recurring,
            durable,
            next_fire_at,
        })
    }

    /// A task's next fire, computed once and cached.
    fn schedule(&mut self, task: &ScheduledTask, zone: &dyn CronZone) -> Option<i64> {
        if let Some(next) = self.next_fire.get(&task.id) {
            return (*next != i64::MAX).then_some(*next);
        }
        let next = match &task.kind {
            TaskKind::Wakeup { scheduled_for, .. } => Some(*scheduled_for),
            TaskKind::Cron {
                recurring: true, ..
            } => recurring_fire_at(
                &task.cron,
                task.last_fired_at.unwrap_or(task.created_at),
                &task.id,
                &self.config,
                zone,
            ),
            TaskKind::Cron { .. } => {
                one_shot_fire_at(&task.cron, task.created_at, &task.id, &self.config, zone)
            }
        };
        self.next_fire
            .insert(task.id.clone(), next.unwrap_or(i64::MAX));
        next
    }

    /// Remove a task or wakeup by id.
    pub fn delete(&mut self, id: &str) -> Option<ScheduledTask> {
        let index = self.tasks.iter().position(|task| task.id == id)?;
        let task = self.tasks.remove(index);
        self.next_fire.remove(id);
        if task.is_durable() {
            self.durable_dirty = true;
        }
        Some(task)
    }

    /// Every task, in creation order.
    pub fn list(&mut self, zone: &dyn CronZone) -> Vec<TaskView> {
        let tasks = self.tasks.clone();
        tasks
            .iter()
            .map(|task| {
                let next_fire_at = self.schedule(task, zone);
                let (kind, recurring, durable, reason) = match &task.kind {
                    TaskKind::Cron {
                        recurring,
                        durable,
                        origin,
                    } => (
                        match origin {
                            TaskOrigin::Model => "cron",
                            TaskOrigin::Loop => "loop",
                        },
                        *recurring,
                        *durable,
                        None,
                    ),
                    TaskKind::Wakeup { reason, .. } => ("wakeup", false, false, reason.clone()),
                };
                TaskView {
                    id: task.id.clone(),
                    cron: task.cron.clone(),
                    human_schedule: human_schedule(&task.cron),
                    prompt: task.prompt.clone(),
                    kind,
                    recurring,
                    durable,
                    created_at: task.created_at,
                    last_fired_at: task.last_fired_at,
                    next_fire_at,
                    reason,
                }
            })
            .collect()
    }

    /// The earliest pending fire.
    pub fn next_due_at(&mut self, zone: &dyn CronZone) -> Option<i64> {
        let tasks = self.tasks.clone();
        tasks
            .iter()
            .filter_map(|task| self.schedule(task, zone))
            .min()
    }

    /// Take every task due at `now` (call only while the session is idle):
    /// recurring tasks re-arm from `now` (no stacking of missed fires),
    /// one-shots, wakeups and aged-out recurring tasks leave the book.
    pub fn take_due(&mut self, now: i64, zone: &dyn CronZone) -> Vec<Fire> {
        let mut fires = Vec::new();
        let tasks = self.tasks.clone();
        let mut remove = Vec::new();
        for task in &tasks {
            let Some(due) = self.schedule(task, zone) else {
                continue;
            };
            if now < due {
                continue;
            }
            let stamp = local_stamp(now, zone);
            match &task.kind {
                TaskKind::Cron { recurring, .. } => {
                    let final_fire = !*recurring || aged_out(task.created_at, now, &self.config);
                    fires.push(Fire {
                        task_id: task.id.clone(),
                        prompt: task.prompt.clone(),
                        cron: task.cron.clone(),
                        kind: FireKind::Cron {
                            recurring: *recurring,
                            final_fire,
                        },
                        header: format!("Running scheduled task ({stamp})"),
                    });
                    if final_fire {
                        remove.push(task.id.clone());
                    } else {
                        let next = recurring_fire_at(&task.cron, now, &task.id, &self.config, zone);
                        self.next_fire
                            .insert(task.id.clone(), next.unwrap_or(i64::MAX));
                        if let Some(live) = self.tasks.iter_mut().find(|live| live.id == task.id) {
                            live.last_fired_at = Some(now);
                        }
                        if task.is_durable() {
                            self.durable_dirty = true;
                        }
                    }
                }
                TaskKind::Wakeup { noop, .. } => {
                    let streak = self.note_tick(*noop, now);
                    let mut header = format!("Resuming /loop wakeup ({stamp})");
                    if streak > 0 {
                        let since = self.loops.streak_since.unwrap_or(now);
                        let ticks = if streak == 1 { "tick" } else { "ticks" };
                        header = format!(
                            "{header} · {streak} no-op {ticks} since {}",
                            local_stamp(since, zone)
                        );
                    }
                    fires.push(Fire {
                        task_id: task.id.clone(),
                        prompt: task.prompt.clone(),
                        cron: task.cron.clone(),
                        kind: FireKind::Wakeup {
                            noop_streak: streak,
                        },
                        header,
                    });
                    self.loops.tick_in_flight = Some(task.prompt.clone());
                    remove.push(task.id.clone());
                }
            }
        }
        for id in remove {
            self.delete(&id);
        }
        fires
    }

    /// Count the quiet streak: a wakeup armed with `noop=True` extends it.
    fn note_tick(&mut self, noop: bool, now: i64) -> u32 {
        if noop {
            if self.loops.noop_streak == 0 {
                self.loops.streak_since = Some(now);
            }
            self.loops.noop_streak += 1;
        } else {
            self.loops.noop_streak = 0;
            self.loops.streak_since = None;
        }
        self.loops.noop_streak
    }

    /// `rlm.schedule_wakeup(delay_seconds, prompt, reason, noop=...)`.
    pub fn schedule_wakeup(
        &mut self,
        delay_seconds: f64,
        prompt: &str,
        reason: Option<&str>,
        noop: bool,
        now: i64,
        zone: &dyn CronZone,
    ) -> WakeupOutcome {
        self.arm_wakeup(delay_seconds, prompt, reason, noop, now, zone, false)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the model's wakeup fields plus the clock, the zone and the keepalive flag"
    )]
    fn arm_wakeup(
        &mut self,
        delay_seconds: f64,
        prompt: &str,
        reason: Option<&str>,
        noop: bool,
        now: i64,
        zone: &dyn CronZone,
        keepalive: bool,
    ) -> WakeupOutcome {
        if !keepalive {
            self.loops.keepalives_in_a_row = 0;
        }
        // One pending wakeup at a time: a new one supersedes the old.
        let superseded: Vec<String> = self
            .tasks
            .iter()
            .filter(|task| task.is_wakeup())
            .map(|task| task.id.clone())
            .collect();
        for id in superseded {
            self.delete(&id);
        }
        let chain = self.loops.chains.get(prompt).copied();
        let started_at = match chain {
            Some(chain) if now <= chain.last_scheduled_for + CHAIN_STALE_MS => chain.started_at,
            _ => now,
        };
        if aged_out(started_at, now, &self.config) {
            self.loops.chains.remove(prompt);
            self.end_loop();
            return WakeupOutcome::AgedOut;
        }
        let (clamped_seconds, was_clamped) = clamp_delay(delay_seconds);
        let scheduled_for = now + clamped_seconds * 1000;
        let wall = zone.local(scheduled_for);
        let id = self.mint_id();
        self.tasks.push(ScheduledTask {
            id: id.clone(),
            cron: {
                use chrono::Timelike;
                format!("{} {} * * *", wall.minute(), wall.hour())
            },
            prompt: prompt.to_string(),
            created_at: now,
            last_fired_at: None,
            kind: TaskKind::Wakeup {
                scheduled_for,
                reason: reason.map(str::to_string),
                keepalive,
                noop,
            },
        });
        self.next_fire.insert(id, scheduled_for);
        self.loops.chains.insert(
            prompt.to_string(),
            Chain {
                started_at,
                last_scheduled_for: scheduled_for,
            },
        );
        WakeupOutcome::Scheduled {
            scheduled_for,
            clamped_seconds,
            was_clamped,
        }
    }

    fn cancel_wakeups(&mut self) -> usize {
        let ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|task| task.is_wakeup())
            .map(|task| task.id.clone())
            .collect();
        for id in &ids {
            self.delete(id);
        }
        ids.len()
    }

    fn end_loop(&mut self) {
        self.loops.tick_in_flight = None;
        self.loops.keepalives_in_a_row = 0;
        self.loops.noop_streak = 0;
        self.loops.streak_since = None;
    }

    /// `rlm.schedule_wakeup(stop=True)`: the self-paced loop ends; a
    /// fixed-interval loop's cron task is not touched. Returns how many
    /// pending wakeups it cancelled.
    pub fn stop_dynamic_loop(&mut self) -> usize {
        let cancelled = self.cancel_wakeups();
        self.end_loop();
        cancelled
    }

    /// `/loop stop`: every wakeup and every fixed `/loop` task.
    pub fn stop_loops(&mut self) -> usize {
        let ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|task| task.started_by_loop())
            .map(|task| task.id.clone())
            .collect();
        for id in &ids {
            self.delete(id);
        }
        self.end_loop();
        ids.len()
    }

    /// The user interrupted a turn: pending self-paced wakeups are
    /// cancelled (Claude Code's abort rule; recurring tasks keep running).
    pub fn user_abort(&mut self) -> usize {
        let cancelled = self.cancel_wakeups();
        self.end_loop();
        cancelled
    }

    /// A delivered fire's turn settled: a loop tick that did not re-arm
    /// gets one host keepalive, then the loop ends.
    pub fn tick_settled(&mut self, now: i64, keepalive: bool, zone: &dyn CronZone) -> TickSettle {
        let Some(prompt) = self.loops.tick_in_flight.take() else {
            return TickSettle::NoTick;
        };
        if self.has_pending_wakeup() {
            return TickSettle::Rearmed;
        }
        if !keepalive || self.loops.keepalives_in_a_row >= KEEPALIVE_BUDGET {
            self.end_loop();
            return TickSettle::Ended;
        }
        match self.arm_wakeup(
            KEEPALIVE_DELAY_SECONDS,
            &prompt,
            None,
            false,
            now,
            zone,
            true,
        ) {
            WakeupOutcome::Scheduled { scheduled_for, .. } => {
                self.loops.keepalives_in_a_row += 1;
                TickSettle::KeepaliveArmed { scheduled_for }
            }
            WakeupOutcome::AgedOut => TickSettle::Ended,
        }
    }

    /// Expand a fired prompt: the autonomous sentinels become the preamble
    /// (first delivery) or the short reminder; `/loop <input>` (a
    /// self-paced loop's re-entry) becomes the dynamic `/loop` body.
    pub fn resolve_fire_prompt(&mut self, prompt: &str, persistent: bool) -> String {
        if prompts::is_autonomous_sentinel(prompt) {
            let text =
                prompts::resolve_autonomous_fire(prompt, self.loops.preamble_delivered, persistent);
            self.loops.preamble_delivered = true;
            return text;
        }
        if let Some(input) = loop_reentry(prompt) {
            if input.is_empty() {
                self.loops.preamble_delivered = true;
                return prompts::autonomous_dynamic_instructions(persistent);
            }
            return prompts::dynamic_loop_instructions(&input);
        }
        prompt.to_string()
    }

    /// The `/loop` command delivered the autonomous preamble itself.
    pub fn note_preamble_delivered(&mut self) {
        self.loops.preamble_delivered = true;
    }

    /// The durable tasks to persist, when they changed since the last call.
    pub fn take_durable_changes(&mut self) -> Option<Vec<DurableTask>> {
        if !std::mem::take(&mut self.durable_dirty) {
            return None;
        }
        Some(
            self.tasks
                .iter()
                .filter(|task| task.is_durable())
                .map(|task| DurableTask {
                    id: task.id.clone(),
                    cron: task.cron.clone(),
                    prompt: task.prompt.clone(),
                    created_at: task.created_at,
                    last_fired_at: task.last_fired_at,
                    recurring: task.is_recurring(),
                })
                .collect(),
        )
    }

    /// Load a resumed session's durable tasks. One-shots whose time passed
    /// while the session was not running come back as one missed-task fire
    /// (and leave the file); recurring tasks catch up with one fire.
    pub fn load_durable(
        &mut self,
        records: Vec<DurableTask>,
        now: i64,
        zone: &dyn CronZone,
    ) -> Option<Fire> {
        let mut missed = Vec::new();
        for record in records {
            if self.tasks.iter().any(|task| task.id == record.id) || parse(&record.cron).is_none() {
                continue;
            }
            if !record.recurring
                && next_fire(&record.cron, record.created_at, zone).is_some_and(|at| at < now)
            {
                missed.push(record);
                continue;
            }
            self.tasks.push(ScheduledTask {
                id: record.id,
                cron: record.cron,
                prompt: record.prompt,
                created_at: record.created_at,
                last_fired_at: record.last_fired_at,
                kind: TaskKind::Cron {
                    recurring: record.recurring,
                    durable: true,
                    origin: TaskOrigin::Model,
                },
            });
        }
        if missed.is_empty() {
            return None;
        }
        self.durable_dirty = true;
        let entries: Vec<String> = missed
            .iter()
            .map(|task| {
                format!(
                    "[{}, created {}]\n{}",
                    human_schedule(&task.cron),
                    local_stamp(task.created_at, zone),
                    task.prompt
                )
            })
            .collect();
        Some(Fire {
            task_id: missed
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>()
                .join(","),
            prompt: prompts::missed_tasks_notice(&entries),
            cron: String::new(),
            kind: FireKind::Missed,
            header: format!(
                "Missed scheduled task{} ({})",
                if missed.len() > 1 { "s" } else { "" },
                local_stamp(now, zone)
            ),
        })
    }
}

/// `/loop <input>` as a fired prompt: the input after the command.
fn loop_reentry(prompt: &str) -> Option<String> {
    let rest = prompt.trim().strip_prefix("/loop")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    // A re-entry is always self-paced: an interval typed into it is dropped
    // (a fixed loop is a cron task, never a wakeup's prompt).
    match super::loop_command::parse_loop(rest) {
        Ok(
            super::loop_command::LoopPlan::Fixed { prompt, .. }
            | super::loop_command::LoopPlan::Dynamic { prompt },
        ) => Some(prompt),
        Ok(
            super::loop_command::LoopPlan::AutonomousFixed { .. }
            | super::loop_command::LoopPlan::AutonomousDynamic,
        )
        | Err(_) => Some(String::new()),
        Ok(super::loop_command::LoopPlan::Stop) => None,
    }
}

/// Claude Code's clamp: round, then hold to [60, 3600]; a non-finite delay
/// is clamped (NaN and -inf to the floor, +inf to the ceiling).
#[must_use]
pub fn clamp_delay(delay_seconds: f64) -> (i64, bool) {
    let rounded = if delay_seconds.is_nan() || delay_seconds == f64::NEG_INFINITY {
        MIN_WAKEUP_SECONDS
    } else if delay_seconds == f64::INFINITY {
        MAX_WAKEUP_SECONDS
    } else {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the value is clamped to [60, 3600] right after; saturation is fine"
        )]
        let rounded = delay_seconds.round() as i64;
        rounded
    };
    let clamped = rounded.clamp(MIN_WAKEUP_SECONDS, MAX_WAKEUP_SECONDS);
    (clamped, !delay_seconds.is_finite() || clamped != rounded)
}

#[cfg(test)]
#[path = "book_tests.rs"]
mod tests;
