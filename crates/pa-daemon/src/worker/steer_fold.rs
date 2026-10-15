//! The worker's [`SteeringFold`]: at each boundary of the running turn,
//! the steering lane's front items that can join it leave the lane and are
//! admitted into the running turn by the session (Claude Code's mid-turn
//! delivery), instead of ending the run and starting the next one on them.
//!
//! An item joins only when it is a plain user steer: queued by a client
//! (visible, ordinary turn policy), text only, no injected row, no session
//! command, no agent-message or scheduler origin. Everything else keeps the
//! stop-and-restart delivery. A folded item's waiter (`prompt_and_wait`)
//! settles with the run it joined; its admission id counts as running. A
//! steer the session refused (an installed feature's prompt review) goes
//! back to the lane's front and the run ends for it, so the restart's
//! admission refuses it the usual, visible way.

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

/// Whether `item` can join a running turn.
fn joinable(item: &QueuedItem) -> bool {
    item.queue_visible
        && item.policy == TurnPolicy::Queued
        && item.custom_message.is_none()
        && item.images.is_empty()
        && item.agent_message.is_none()
        && item.queue_key.is_none()
        && crate::session_commands::parse_prompt_session_command(&item.message).is_none()
}

/// The worker's steering lane as the session's [`SteeringFold`].
pub(crate) struct WorkerSteeringFold {
    pub(crate) core: Arc<Mutex<SessionCore>>,
    pub(crate) events: Arc<super::EventPump>,
    pub(crate) recovery: Arc<Mutex<Option<WorkerRecoveryJournal>>>,
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

    #[test]
    fn plain_steers_fold_until_one_cannot_and_a_refusal_goes_back_to_the_front() {
        let core = Arc::new(Mutex::new(SessionCore::test_core(None, "/tmp".to_string())));
        let fold = WorkerSteeringFold {
            core: Arc::clone(&core),
            events: Arc::new(super::super::EventPump::new()),
            recovery: Arc::new(Mutex::new(None)),
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
