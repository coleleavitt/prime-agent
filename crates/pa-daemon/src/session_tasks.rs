//! The worker's scheduler for the session's Claude Code tasks
//! (`rlm.cron.*`, `rlm.schedule_wakeup`, `/loop`; the book is
//! `pa_core::cron::tasks`). The daemon owns time: sessions outlive their
//! clients, so the worker, not a client, wakes for a due task.
//!
//! A fire never reaches the session mid-turn: the scheduler takes what is
//! due only while the session is idle (no turn, compaction, queued input,
//! user bash or input pause) and admits it on the follow-up lane as an
//! injected `scheduled_task_fire` row, the lane the background notices wake
//! an idle session through. While the session is busy the due task waits
//! for the turn runner's park signal; a recurring task that missed several
//! periods fires once (the book re-arms it from the fire). A delivered
//! self-paced loop tick reports its settle back to the book, which arms the
//! keepalive wakeup when the model did not re-arm.

use std::sync::{Arc, Mutex};

use pa_core::cron::tasks::{
    Fire,
    FireKind,
    SCHEDULED_TASK_FIRE_CUSTOM_TYPE,
    SessionTasks,
    TickSettle,
};
use pa_types::sync::MutexExt;
use serde_json::json;
use tokio::sync::{Notify, oneshot};

use crate::worker::{QueuedItem, SessionCore, TurnSettle};

/// How often a due task re-checks a busy session between runner park
/// signals (a user bash, a compaction or an input pause settles without
/// one). Claude Code's scheduler polls every second too.
const BUSY_RECHECK: std::time::Duration = std::time::Duration::from_secs(1);

/// The scheduler: the session's book plus the worker state that decides
/// idleness and receives fires.
pub(crate) struct SessionTaskScheduler {
    tasks: Arc<SessionTasks>,
    core: Arc<Mutex<SessionCore>>,
    work_notify: Arc<Notify>,
    idle_notify: Arc<Notify>,
    user_bash: Arc<crate::user_bash::UserBash>,
    input_pauses: crate::session_input_pause::InputPauseTable,
    recovery: Arc<Mutex<Option<crate::journal::WorkerRecoveryJournal>>>,
    /// Fires that wait for the next idle pass (a resumed session's missed
    /// one-shot notice).
    pending: Mutex<Vec<Fire>>,
    timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for SessionTaskScheduler {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.lock_or_recover().take() {
            timer.abort();
        }
    }
}

/// The worker state the scheduler reads and writes.
pub(crate) struct SchedulerWiring {
    pub(crate) core: Arc<Mutex<SessionCore>>,
    pub(crate) work_notify: Arc<Notify>,
    pub(crate) idle_notify: Arc<Notify>,
    pub(crate) user_bash: Arc<crate::user_bash::UserBash>,
    pub(crate) input_pauses: crate::session_input_pause::InputPauseTable,
    pub(crate) recovery: Arc<Mutex<Option<crate::journal::WorkerRecoveryJournal>>>,
}

impl SessionTaskScheduler {
    pub(crate) fn new(tasks: Arc<SessionTasks>, wiring: SchedulerWiring) -> Arc<Self> {
        Arc::new(Self {
            tasks,
            core: wiring.core,
            work_notify: wiring.work_notify,
            idle_notify: wiring.idle_notify,
            user_bash: wiring.user_bash,
            input_pauses: wiring.input_pauses,
            recovery: wiring.recovery,
            pending: Mutex::new(Vec::new()),
            timer: Mutex::new(None),
        })
    }

    pub(crate) fn tasks(&self) -> &Arc<SessionTasks> {
        &self.tasks
    }

    /// Whether a fire may start a turn now.
    pub(crate) fn idle(&self) -> bool {
        if self.user_bash.is_running() || self.input_pauses.paused() {
            return false;
        }
        let core = self.core.lock_or_recover();
        core.created
            && !core.shutdown_requested
            && !core.busy
            && !core.compacting
            && core.steering.is_empty()
            && core.follow_up.is_empty()
            && core.pending_next_turn.is_empty()
    }

    /// Whether anything is scheduled (it keeps the worker resident).
    pub(crate) fn has_pending(&self) -> bool {
        self.tasks.has_pending() || !self.pending.lock_or_recover().is_empty()
    }

    /// Bind the live session (create and every replacement): its durable
    /// tasks load from its artifact partition; a different session than the
    /// bound one starts from an empty book.
    pub(crate) fn bind(self: &Arc<Self>) {
        let (session_id, artifacts) = {
            let core = self.core.lock_or_recover();
            let subagent = core.runtime_kind == "subagent";
            match core.store.as_ref() {
                Some(store) if !store.path.as_os_str().is_empty() => {
                    let artifacts = if subagent {
                        Err("durable tasks are not supported for subagents".to_string())
                    } else {
                        crate::scheduled_jobs::session_artifact_dir(&store.path, store.session_id())
                            .ok_or_else(|| "durable tasks need a saved session".to_string())
                    };
                    (store.session_id().to_string(), artifacts)
                }
                Some(store) => (
                    store.session_id().to_string(),
                    Err("durable tasks need a saved session".to_string()),
                ),
                None => (
                    core.active_session_id.clone(),
                    Err("durable tasks need a saved session".to_string()),
                ),
            }
        };
        if let Some(missed) = self.tasks.bind(&session_id, artifacts) {
            self.pending.lock_or_recover().push(missed);
        }
        self.start();
    }

    /// Start the timer task once (on the worker's runtime).
    fn start(self: &Arc<Self>) {
        let mut timer = self.timer.lock_or_recover();
        if timer.is_some() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let weak = Arc::downgrade(self);
        *timer = Some(runtime.spawn(run_timer(weak)));
    }

    /// The user interrupted the session: pending self-paced wakeups end
    /// (recurring tasks keep running).
    pub(crate) fn user_abort(&self) {
        self.tasks.user_abort();
    }

    /// One scheduling pass: when the session is idle, deliver every due
    /// task. Returns how many fires it admitted.
    pub(crate) fn deliver_due(self: &Arc<Self>) -> usize {
        if !self.idle() {
            return 0;
        }
        let mut fires = std::mem::take(&mut *self.pending.lock_or_recover());
        fires.extend(self.tasks.take_due());
        let count = fires.len();
        for fire in &fires {
            self.deliver(fire);
        }
        count
    }

    /// Admit one fire on the follow-up lane as an injected row.
    fn deliver(self: &Arc<Self>, fire: &Fire) {
        let content = self.tasks.resolve_fire_prompt(&fire.prompt);
        let (kind, details) = match &fire.kind {
            FireKind::Cron {
                recurring,
                final_fire,
            } => (
                "cron",
                json!({ "recurring": recurring, "finalFire": final_fire }),
            ),
            FireKind::Wakeup { noop_streak } => ("wakeup", json!({ "noopStreak": noop_streak })),
            FireKind::Missed => ("missed", json!({})),
        };
        let mut details = details;
        details["taskId"] = json!(fire.task_id);
        details["kind"] = json!(kind);
        details["header"] = json!(fire.header);
        if !fire.cron.is_empty() {
            details["cron"] = json!(fire.cron);
            details["humanSchedule"] =
                json!(pa_core::cron::tasks::expr::human_schedule(&fire.cron));
        }
        let row = pa_types::session::CustomMessage {
            custom_type: SCHEDULED_TASK_FIRE_CUSTOM_TYPE.to_string(),
            content: pa_types::ai::UserContent::Text(content.clone()),
            display: true,
            details: Some(details),
            timestamp: u64::try_from(self.tasks.now()).unwrap_or_default(),
            rest: serde_json::Map::default(),
        };
        let first_line = fire.prompt.lines().next().unwrap_or_default();
        let preview = match fire.kind {
            FireKind::Wakeup { .. } => format!("Loop wakeup: {first_line}"),
            FireKind::Cron { .. } | FireKind::Missed => format!("Scheduled task: {first_line}"),
        };
        let (done, settled) = if matches!(fire.kind, FireKind::Wakeup { .. }) {
            let (sender, receiver) = oneshot::channel();
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        {
            let mut core = self.core.lock_or_recover();
            if !core.created || core.shutdown_requested {
                return;
            }
            // A fire is a resume site, like the heartbeat catalog's.
            core.queued_input_suspended = false;
            let queue_key = format!("scheduled_task:{}", fire.task_id);
            core.follow_up
                .retain(|item| item.queue_key.as_deref() != Some(queue_key.as_str()));
            crate::worker::enqueue_priority(
                &mut core.follow_up,
                QueuedItem {
                    priority: crate::worker::QueuePriority::Background,
                    message: content,
                    preview: Some(preview),
                    custom_message: Some(crate::session_commands::custom_message_value(&row)),
                    agent_message: None,
                    admission_id: None,
                    images: Vec::new(),
                    queue_key: Some(queue_key),
                    done,
                    queue_visible: true,
                    policy: crate::worker::TurnPolicy::Injected,
                    forced_batch: false,
                    traceparent: None,
                },
            );
        }
        crate::worker::checkpoint_queue_recovery(
            &self.recovery,
            &self.core,
            crate::worker::QueueCheckpoint::Admitted {
                operation: "follow_up_queued",
            },
            None,
        );
        self.work_notify.notify_one();
        if let Some(settled) = settled {
            let scheduler = Arc::downgrade(self);
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let settle = settled.await;
                    if let Some(scheduler) = scheduler.upgrade() {
                        scheduler.tick_settled(settle.as_ref().ok());
                    }
                });
            }
        }
    }

    /// A delivered loop tick's turn settled. A withdrawn tick (the queue was
    /// cleared) ends the loop like an interrupt.
    pub(crate) fn tick_settled(&self, settle: Option<&TurnSettle>) -> TickSettle {
        match settle {
            Some(TurnSettle::Withdrawn(_)) => {
                self.tasks.stop_dynamic_loop();
                TickSettle::Ended
            }
            Some(TurnSettle::Completed | TurnSettle::Aborted | TurnSettle::Failed(_)) | None => {
                self.tasks.tick_settled()
            }
        }
    }
}

/// The timer: sleep to the next fire (or a mutation), and while a due task
/// waits on a busy session, wake on the runner's park signal.
async fn run_timer(scheduler: std::sync::Weak<SessionTaskScheduler>) {
    loop {
        let Some(this) = scheduler.upgrade() else {
            return;
        };
        let tasks = Arc::clone(this.tasks());
        let idle_notify = Arc::clone(&this.idle_notify);
        let changed = tasks.changed().notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let idle = idle_notify.notified();
        tokio::pin!(idle);
        idle.as_mut().enable();
        this.deliver_due();
        let waiting = !this.pending.lock_or_recover().is_empty();
        drop(this);
        let now = tasks.now();
        match tasks.next_due_at() {
            Some(due) if due <= now || waiting => {
                tokio::select! {
                    () = &mut changed => {}
                    () = &mut idle => {}
                    () = tokio::time::sleep(BUSY_RECHECK) => {}
                }
            }
            Some(due) => {
                let delay = std::time::Duration::from_millis(u64::try_from(due - now).unwrap_or(0));
                tokio::select! {
                    () = &mut changed => {}
                    () = tokio::time::sleep(delay) => {}
                }
            }
            None if waiting => {
                tokio::select! {
                    () = &mut changed => {}
                    () = &mut idle => {}
                    () = tokio::time::sleep(BUSY_RECHECK) => {}
                }
            }
            None => changed.await,
        }
    }
}

#[cfg(test)]
#[path = "session_tasks_tests.rs"]
mod tests;
