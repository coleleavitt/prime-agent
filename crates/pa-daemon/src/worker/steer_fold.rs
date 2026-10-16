//! The worker's [`SteeringFold`]: at each boundary of the running turn,
//! the steering lane's front items that can join it leave the lane and are
//! admitted into the running turn by the session (Claude Code's mid-turn
//! delivery), instead of ending the run and starting the next one on them.
//!
//! An item joins when it is a plain user steer: queued by a client
//! (visible, ordinary turn policy), text only, no injected row, no session
//! command, no agent-message or scheduler origin. A host notice joins too,
//! as its own row (Claude Code folds its task notifications in after a tool
//! batch): a finished background command or a watch event queued on a busy
//! session ([`FOLDABLE_NOTICE_TYPES`]). Everything else (heartbeats,
//! injected rows, agent messages) keeps the stop-and-restart delivery. A folded item's waiter (`prompt_and_wait`)
//! settles with the run it joined; its admission id counts as running. A
//! steer the session refused (an installed feature's prompt review) goes
//! back to the lane's front and the run ends for it, so the restart's
//! admission refuses it the usual, visible way. A held input pause (a
//! client editing the queue, an agent message's admission committing)
//! keeps the lane where it is.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pa_core::session_engine::steering_fold::{FoldedSteer, SteeringFold};
use pa_types::sync::MutexExt;
use tokio::sync::oneshot;

use super::queue::{
    QueueCheckpoint,
    QueuedItem,
    TurnPolicy,
    TurnSettle,
    checkpoint_queue_recovery,
};
use super::session_core::SessionCore;
use super::summary::session_snapshot;
use crate::journal::WorkerRecoveryJournal;

/// The folding state the session core carries.
#[derive(Default)]
pub(crate) struct FoldState {
    /// Items taken from the lane, awaiting the session's verdict, by ticket.
    taken: HashMap<u64, QueuedItem>,
    /// Waiters of folded items: they settle with the running turn.
    pub(crate) done: Vec<oneshot::Sender<TurnSettle>>,
    /// Admission ids of folded items: cleared with the running turn.
    pub(crate) admissions: Vec<String>,
    /// The lane's front item was refused by the fold: it waits for the
    /// restart (cleared when a turn picks up the lane).
    pub(crate) front_refused: bool,
}

/// The host notice rows that join a running turn: a finished background
/// command, a watch event, a monitor's lines, a background cell's end.
pub(crate) const FOLDABLE_NOTICE_TYPES: [&str; 4] = [
    pa_core::session_engine::messages::ASYNC_BASH_COMPLETION_CUSTOM_TYPE,
    crate::agent_watch::AGENT_WATCH_NOTICE_CUSTOM_TYPE,
    pa_core::session_engine::messages::MONITOR_EVENT_CUSTOM_TYPE,
    pa_core::session_engine::messages::BACKGROUND_CELL_CUSTOM_TYPE,
];

/// Whether `item` can join a running turn.
fn joinable(item: &QueuedItem) -> bool {
    let plain = item.queue_visible
        && item.policy == TurnPolicy::Queued
        && item.images.is_empty()
        && item.agent_message.is_none()
        && item.queue_key.is_none();
    plain
        && match &item.custom_message {
            None => crate::session_commands::parse_prompt_session_command(&item.message).is_none(),
            Some(row) => row
                .get("customType")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| FOLDABLE_NOTICE_TYPES.contains(&kind)),
        }
}

/// The worker's steering lane as the session's [`SteeringFold`].
pub(crate) struct WorkerSteeringFold {
    pub(crate) core: Arc<Mutex<SessionCore>>,
    pub(crate) events: Arc<super::EventPump>,
    pub(crate) recovery: Arc<Mutex<Option<WorkerRecoveryJournal>>>,
    pub(crate) input_pauses: crate::session_input_pause::InputPauseTable,
    pub(crate) next_ticket: AtomicU64,
}

impl WorkerSteeringFold {
    /// The lane changed: journal it and tell the clients.
    fn publish(&self) {
        checkpoint_queue_recovery(
            &self.recovery,
            &self.core,
            QueueCheckpoint::Admitted {
                operation: "steer_folded",
            },
            None,
        );
        let mut core = self.core.lock_or_recover();
        let snapshot = session_snapshot(&core);
        if core.last_action_snapshot.as_ref() == Some(&snapshot) {
            return;
        }
        core.last_action_snapshot = Some(snapshot.clone());
        let sequence = core.last_event_sequence + 1;
        core.last_event_sequence = sequence;
        let meta = crate::protocol::create_daemon_event_meta(
            &core.active_session_id,
            sequence,
            None,
            Some(&core.generation),
        );
        let outbound = crate::protocol::DaemonOutbound::SessionEvent {
            active_session_id: core.active_session_id.clone(),
            event: serde_json::json!({ "type": "session_action_update", "actions": snapshot }),
            meta: Some(meta),
            rest: serde_json::Map::default(),
        };
        drop(core);
        if let Ok(payload) = serde_json::to_vec(&outbound) {
            self.events
                .send(super::OutboundFrame::session_event(payload));
        }
    }
}

impl SteeringFold for WorkerSteeringFold {
    fn take(&self) -> Vec<FoldedSteer> {
        let mut taken = Vec::new();
        if self.input_pauses.paused() {
            return taken;
        }
        {
            let mut core = self.core.lock_or_recover();
            // Only a running turn takes steers; a paused or aborting lane keeps them.
            if !core.busy
                || core.abort_requested
                || core.queued_input_suspended
                || core.steer_fold.front_refused
            {
                return taken;
            }
            while core.steering.front().is_some_and(joinable) {
                let Some(item) = core.steering.pop_front() else {
                    break;
                };
                let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
                if let Some(admission) = &item.admission_id {
                    core.running_admission_ids.insert(admission.clone());
                }
                taken.push(FoldedSteer {
                    text: item.message.clone(),
                    ticket,
                    custom: item.custom_message.clone(),
                });
                core.steer_fold.taken.insert(ticket, item);
            }
        }
        if !taken.is_empty() {
            self.publish();
        }
        taken
    }

    fn settled(&self, steer: FoldedSteer, outcome: Result<(), String>) {
        let mut core = self.core.lock_or_recover();
        let Some(mut item) = core.steer_fold.taken.remove(&steer.ticket) else {
            return;
        };
        match outcome {
            Ok(()) => {
                if let Some(done) = item.done.take() {
                    core.steer_fold.done.push(done);
                }
                if let Some(admission) = item.admission_id.take() {
                    core.steer_fold.admissions.push(admission);
                }
            }
            Err(reason) => {
                eprintln!("a queued steer was refused mid-run; it waits for the restart: {reason}");
                if let Some(admission) = &item.admission_id {
                    core.running_admission_ids.remove(admission);
                }
                core.steering.push_front(item);
                core.steer_fold.front_refused = true;
                drop(core);
                self.publish();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(message: &str) -> QueuedItem {
        QueuedItem {
            message: message.to_string(),
            priority: super::super::queue::QueuePriority::Human,
            preview: None,
            custom_message: None,
            agent_message: None,
            queue_key: None,
            admission_id: None,
            images: Vec::new(),
            done: None,
            queue_visible: true,
            policy: TurnPolicy::Queued,
            forced_batch: false,
            traceparent: None,
        }
    }

    #[test]
    fn only_plain_user_steers_join() {
        assert!(joinable(&item("also check the tests")));
        assert!(!joinable(&item("/compact")));
        let mut injected = item("x");
        injected.custom_message = Some(serde_json::json!({}));
        assert!(!joinable(&injected));
        let mut direct = item("x");
        direct.policy = TurnPolicy::Direct;
        assert!(!joinable(&direct));
        let mut invisible = item("x");
        invisible.queue_visible = false;
        assert!(!joinable(&invisible));
        let mut keyed = item("x");
        keyed.queue_key = Some("cron".to_string());
        assert!(!joinable(&keyed));
        let mut agent = item("x");
        agent.agent_message = Some("x".to_string());
        assert!(!joinable(&agent));
    }

    /// A monitor's batches queue as one notice row per monitor until the
    /// session takes it: a later batch merges its lines in, the end too.
    #[test]
    fn monitor_batches_merge_into_their_undelivered_row_and_fold() {
        use pa_core::session_engine::messages::MonitorEvent;
        let core = Arc::new(Mutex::new(SessionCore::test_core(None, "/tmp".to_string())));
        let recovery = Mutex::new(None);
        let notify = Arc::new(tokio::sync::Notify::new());
        core.lock().unwrap().busy = true;
        let batch = |id: &str, line: &str, end: Option<serde_json::Value>| MonitorEvent {
            monitor_id: id.to_string(),
            description: "tests".to_string(),
            lines: vec![line.to_string()],
            dropped: 0,
            end,
        };
        let admit = |event| {
            super::super::queue::admit_monitor_event(&recovery, &core, &notify, event, || false);
        };
        admit(batch("m1", "first", None));
        admit(batch("m2", "other", None));
        admit(batch(
            "m1",
            "second",
            Some(serde_json::json!({"reason": "exited", "exitCode": 0})),
        ));
        {
            let core = core.lock().unwrap();
            assert_eq!(core.steering.len(), 2, "one row per monitor");
            let row = core.steering[0].custom_message.as_ref().unwrap();
            assert_eq!(
                row["details"]["lines"],
                serde_json::json!(["first", "second"])
            );
            assert_eq!(row["details"]["end"]["reason"], "exited");
            assert!(
                core.steering[0]
                    .message
                    .contains("ended: the command exited (code 0)")
            );
            assert!(joinable(&core.steering[0]));
        }
    }

    /// A finished background command's notice and a watch event join the
    /// running turn as their rows; other host rows keep the restart.
    #[test]
    fn host_notices_join_and_other_host_rows_do_not() {
        let notice = |custom_type: &str| {
            let mut item = item("[bash-done pid:1 exit:0]");
            item.priority = super::super::queue::QueuePriority::Background;
            item.custom_message = Some(serde_json::json!({
                "role": "custom",
                "customType": custom_type,
                "content": "x",
                "display": true,
                "timestamp": 0,
            }));
            item
        };
        assert!(joinable(&notice("async_bash_completion")));
        assert!(joinable(&notice("agent_watch_notice")));
        assert!(!joinable(&notice("heartbeat_prompt")));
        // An idle session's notice is injected, not queued: the runner takes it.
        let mut injected = notice("async_bash_completion");
        injected.policy = TurnPolicy::Injected;
        injected.queue_visible = false;
        assert!(!joinable(&injected));

        let core = Arc::new(Mutex::new(SessionCore::test_core(None, "/tmp".to_string())));
        let fold = WorkerSteeringFold {
            core: Arc::clone(&core),
            events: Arc::new(super::super::EventPump::new()),
            recovery: Arc::new(Mutex::new(None)),
            input_pauses: crate::session_input_pause::InputPauseTable::new(),
            next_ticket: AtomicU64::new(1),
        };
        {
            let mut core = core.lock().unwrap();
            core.busy = true;
            core.steering.push_back(notice("async_bash_completion"));
        }
        let taken = fold.take();
        assert_eq!(taken.len(), 1);
        assert_eq!(
            taken[0].custom.as_ref().unwrap()["customType"],
            "async_bash_completion"
        );
    }

    #[test]
    fn plain_steers_fold_until_one_cannot_and_a_refusal_goes_back_to_the_front() {
        let core = Arc::new(Mutex::new(SessionCore::test_core(None, "/tmp".to_string())));
        let fold = WorkerSteeringFold {
            core: Arc::clone(&core),
            events: Arc::new(super::super::EventPump::new()),
            recovery: Arc::new(Mutex::new(None)),
            input_pauses: crate::session_input_pause::InputPauseTable::new(),
            next_ticket: AtomicU64::new(1),
        };
        let (done, mut settled) = oneshot::channel();
        {
            let mut core = core.lock().unwrap();
            let mut first = item("first steer");
            first.done = Some(done);
            first.admission_id = Some("adm-1".to_string());
            core.steering
                .extend([first, item("second steer"), item("/compact"), item("after")]);
            // Idle: nothing folds.
        }
        assert!(fold.take().is_empty());
        core.lock().unwrap().busy = true;
        // A held pause (a client editing the queue) keeps the lane queued.
        let pause = fold.input_pauses.acquire("s", "client", "queue-edit");
        assert!(fold.take().is_empty());
        assert_eq!(
            fold.input_pauses.release(&pause, "client", "s"),
            crate::session_input_pause::ReleaseOutcome::Released
        );
        let taken = fold.take();
        let texts: Vec<&str> = taken.iter().map(|steer| steer.text.as_str()).collect();
        assert_eq!(texts, ["first steer", "second steer"]);
        assert!(core.lock().unwrap().running_admission_ids.contains("adm-1"));
        let mut taken = taken.into_iter();
        fold.settled(taken.next().unwrap(), Ok(()));
        fold.settled(taken.next().unwrap(), Err("refused".to_string()));
        {
            let core = core.lock().unwrap();
            let lane: Vec<&str> = core
                .steering
                .iter()
                .map(|item| item.message.as_str())
                .collect();
            assert_eq!(lane, ["second steer", "/compact", "after"]);
            assert!(core.steer_fold.front_refused);
            assert_eq!(core.steer_fold.admissions, ["adm-1"]);
            assert_eq!(core.steer_fold.done.len(), 1);
        }
        // The refused front waits for the restart.
        assert!(fold.take().is_empty());
        // The admitted steer's waiter settles with the run.
        let done = core.lock().unwrap().steer_fold.done.pop().unwrap();
        let _ = done.send(TurnSettle::Completed);
        assert!(matches!(settled.try_recv(), Ok(TurnSettle::Completed)));
    }
}
