//! Session-event subscribers: the send-time routing index (TS `handleWorkerFrame`:
//! the delivery set is the session's attached clients, evaluated in the socket-write
//! pass). Publishers resolve the set under one lock and enqueue into per-connection
//! bounded queues; a full queue drops with one log line per stall cycle and marks the
//! session lagged for that connection, so the connection's writer sends one
//! [`SESSION_RESYNC_REQUIRED`] frame once its queue drains (TS daemons queue a
//! `resync` catch-up snapshot on backpressure and send it on `drain`).

use pa_types::sync::MutexExt;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};

/// The frame type telling a client it lost session events (its queue overflowed) and
/// must re-fetch the session's state. Rust-only (the TS daemon streamed a `resync`
/// snapshot instead); clients that do not know it ignore it.
pub(crate) const SESSION_RESYNC_REQUIRED: &str = "session_resync_required";

/// The resync frame for one session.
pub(crate) fn session_resync_required_frame(active_session_id: &str) -> Value {
    json!({
        "type": SESSION_RESYNC_REQUIRED,
        "activeSessionId": active_session_id,
        "reason": "lagged",
    })
}

/// The sessions whose frames one connection's queue dropped since its last resync,
/// plus the wake for the connection's writer.
#[derive(Default)]
pub(crate) struct LaggedSessions {
    ids: Mutex<BTreeSet<String>>,
    notify: Notify,
}

impl LaggedSessions {
    fn mark(&self, active_session_id: &str) {
        let inserted = self
            .ids
            .lock_or_recover()
            .insert(active_session_id.to_string());
        if inserted {
            self.notify.notify_one();
        }
    }

    /// Resolves once a session was marked lagged (a stored permit covers a mark that
    /// landed while the writer was busy).
    pub(crate) async fn notified(&self) {
        self.notify.notified().await;
    }
}

/// What the registry holds per connection: the bounded queue and the lag marks.
#[derive(Clone)]
struct SubscriberLink {
    queue: mpsc::Sender<Arc<Value>>,
    lagged: Arc<LaggedSessions>,
}

/// One connection's subscription state: the session list and the bounded queue its
/// targeted frames ride. The list leads the registry on attach and lags on detach, so
/// disconnect cleanup always covers the registry.
pub(crate) struct ClientSubscriptions {
    connection_id: String,
    sessions: Mutex<Vec<String>>,
    link: SubscriberLink,
}

impl ClientSubscriptions {
    pub(crate) fn new(connection_id: String, queue: mpsc::Sender<Arc<Value>>) -> Arc<Self> {
        Arc::new(Self {
            connection_id,
            sessions: Mutex::new(Vec::new()),
            link: SubscriberLink {
                queue,
                lagged: Arc::new(LaggedSessions::default()),
            },
        })
    }

    /// This connection's lag marks (the writer awaits [`LaggedSessions::notified`]).
    pub(crate) fn lagged(&self) -> &LaggedSessions {
        &self.link.lagged
    }

    /// The writer's drain hook, called once this connection's queue is empty: one
    /// [`session_resync_required_frame`] per session it lost frames for (and still has
    /// attached) goes into the queue, behind nothing, so it cannot be dropped. A frame
    /// that does not fit (a publisher refilled the queue meanwhile) keeps its mark for
    /// the next drain.
    pub(crate) fn queue_pending_resyncs(&self) {
        let lagged = std::mem::take(&mut *self.link.lagged.ids.lock_or_recover());
        let attached = self.session_ids();
        for active_session_id in lagged.into_iter().filter(|id| attached.contains(id)) {
            let frame = Arc::new(session_resync_required_frame(&active_session_id));
            if self.link.queue.try_send(frame).is_err() {
                self.link.lagged.mark(&active_session_id);
            }
        }
    }

    /// The attached-session list (pause bookkeeping, detach-on-disconnect
    /// routing): the registry may lag this list momentarily, never lead it.
    pub(crate) fn session_ids(&self) -> Vec<String> {
        self.sessions.lock_or_recover().clone()
    }

    pub(crate) fn contains(&self, active_session_id: &str) -> bool {
        self.sessions
            .lock_or_recover()
            .iter()
            .any(|id| id == active_session_id)
    }

    /// Attach: the session list first (the routing superset), then the
    /// registry — the registry insertion is the delivery boundary.
    pub(crate) fn attach(&self, registry: &SessionSubscribers, active_session_id: &str) {
        {
            let mut sessions = self.sessions.lock_or_recover();
            if !sessions.iter().any(|id| id == active_session_id) {
                sessions.push(active_session_id.to_string());
            }
        }
        registry.register(active_session_id, &self.connection_id, self.link.clone());
    }

    /// Detach: the registry first (delivery stops at the detach instant),
    /// then the session list.
    pub(crate) fn detach(&self, registry: &SessionSubscribers, active_session_id: &str) {
        registry.unregister(active_session_id, &self.connection_id);
        self.sessions
            .lock_or_recover()
            .retain(|id| id != active_session_id);
    }

    /// The stale-id rebind seam: the connection keeps its prior attached-ness
    /// under the current id. The registry's id move is atomic (one lock spans
    /// the unregister and the register). Returns whether it was attached.
    pub(crate) fn rebind(
        &self,
        registry: &SessionSubscribers,
        selector: &str,
        current: &str,
    ) -> bool {
        let was_attached = self.contains(selector);
        if was_attached {
            registry.move_subscription(selector, current, &self.connection_id, self.link.clone());
            let mut sessions = self.sessions.lock_or_recover();
            sessions.retain(|id| id != selector);
            if !sessions.iter().any(|id| id == current) {
                sessions.push(current.to_string());
            }
        }
        was_attached
    }

    /// Disconnect: every list entry's registry subscription goes (the list is the
    /// superset, so an attach-in-flight cannot leak).
    pub(crate) fn detach_all(&self, registry: &SessionSubscribers) {
        for active_session_id in self.sessions.lock_or_recover().clone() {
            registry.unregister(&active_session_id, &self.connection_id);
        }
    }
}

/// One subscriber's queue, with the one-line-per-stall-cycle loss flag.
struct Subscriber {
    link: SubscriberLink,
    logged_full: bool,
}

/// The delivery outcome the publisher must surface: connections whose
/// queue dropped the frame this publish (a stall-cycle transition).
#[derive(Default)]
pub(crate) struct PublishOutcome {
    pub(crate) delivered: usize,
    pub(crate) lagged: Vec<String>,
}

/// The send-time routing index: session id -> connection id -> queue.
pub(crate) struct SessionSubscribers {
    sessions: Mutex<HashMap<String, HashMap<String, Subscriber>>>,
}

impl SessionSubscribers {
    pub(crate) fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn register(&self, active_session_id: &str, connection_id: &str, link: SubscriberLink) {
        let mut sessions = self.sessions.lock_or_recover();
        sessions
            .entry(active_session_id.to_string())
            .or_default()
            .insert(
                connection_id.to_string(),
                Subscriber {
                    link,
                    logged_full: false,
                },
            );
    }

    fn unregister(&self, active_session_id: &str, connection_id: &str) {
        let mut sessions = self.sessions.lock_or_recover();
        if let Some(subscribers) = sessions.get_mut(active_session_id) {
            subscribers.remove(connection_id);
            if subscribers.is_empty() {
                sessions.remove(active_session_id);
            }
        }
    }

    /// Move one connection's subscription between session ids atomically
    /// (the rebind seam): a publisher never observes the connection under
    /// both ids or neither.
    fn move_subscription(&self, from: &str, to: &str, connection_id: &str, link: SubscriberLink) {
        let mut sessions = self.sessions.lock_or_recover();
        if let Some(subscribers) = sessions.get_mut(from) {
            subscribers.remove(connection_id);
            if subscribers.is_empty() {
                sessions.remove(from);
            }
        }
        sessions.entry(to.to_string()).or_default().insert(
            connection_id.to_string(),
            Subscriber {
                link,
                logged_full: false,
            },
        );
    }

    /// The send-time delivery pass: enqueue to every attached connection under the
    /// registry lock. A full queue drops the frame (the stall-cycle lands in the daemon
    /// log) and marks the session lagged for that connection; a closed queue prunes
    /// its entry.
    pub(crate) fn publish(&self, active_session_id: &str, payload: &Arc<Value>) -> PublishOutcome {
        let mut outcome = PublishOutcome::default();
        let mut sessions = self.sessions.lock_or_recover();
        let Some(subscribers) = sessions.get_mut(active_session_id) else {
            return outcome;
        };
        subscribers.retain(|connection_id, subscriber| {
            match subscriber.link.queue.try_send(Arc::clone(payload)) {
                Ok(()) => {
                    outcome.delivered += 1;
                    subscriber.logged_full = false;
                    true
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    subscriber.link.lagged.mark(active_session_id);
                    if !subscriber.logged_full {
                        subscriber.logged_full = true;
                        outcome.lagged.push(connection_id.clone());
                    }
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
        if subscribers.is_empty() {
            sessions.remove(active_session_id);
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(capacity: usize) -> (mpsc::Sender<Arc<Value>>, mpsc::Receiver<Arc<Value>>) {
        tokio::sync::mpsc::channel(capacity)
    }

    fn frame(tag: &str) -> Arc<Value> {
        std::sync::Arc::new(serde_json::json!({ "type": tag }))
    }

    fn drained(rx: &mut mpsc::Receiver<Arc<Value>>) -> Vec<Value> {
        let mut seen = Vec::new();
        while let Ok(payload) = rx.try_recv() {
            seen.push((*payload).clone());
        }
        seen
    }

    #[tokio::test]
    async fn publish_reaches_only_the_attached_connection() {
        let registry = SessionSubscribers::new();
        let (first_tx, mut first_rx) = queue(8);
        let (second_tx, mut second_rx) = queue(8);
        let first = ClientSubscriptions::new("first".into(), first_tx);
        let second = ClientSubscriptions::new("second".into(), second_tx);
        first.attach(&registry, "session-1");
        let outcome = registry.publish("session-1", &frame("hello"));
        assert_eq!(outcome.delivered, 1);
        assert!(outcome.lagged.is_empty());
        // An unattached connection's queue stays empty: the frame never
        // even reaches it (the send-time routing set is the attached set).
        assert!(drained(&mut second_rx).is_empty());
        assert_eq!(drained(&mut first_rx).len(), 1);
        second.attach(&registry, "session-1");
        let outcome = registry.publish("session-1", &frame("again"));
        assert_eq!(outcome.delivered, 2);
        assert_eq!(drained(&mut first_rx).len(), 1);
        assert_eq!(drained(&mut second_rx).len(), 1);
        // A session nobody attached never registers an entry.
        registry.publish("session-2", &frame("nobody"));
        assert!(drained(&mut first_rx).is_empty());
    }

    #[tokio::test]
    async fn detach_stops_delivery_at_the_detach_instant() {
        let registry = SessionSubscribers::new();
        let (tx, mut rx) = queue(8);
        let client = ClientSubscriptions::new("client".into(), tx);
        client.attach(&registry, "session-1");
        client.detach(&registry, "session-1");
        let outcome = registry.publish("session-1", &frame("late"));
        assert_eq!(outcome.delivered, 0);
        assert!(drained(&mut rx).is_empty());
        assert!(!client.contains("session-1"));
    }

    #[tokio::test]
    async fn rebind_moves_the_subscription_between_ids() {
        let registry = SessionSubscribers::new();
        let (tx, mut rx) = queue(8);
        let client = ClientSubscriptions::new("client".into(), tx);
        client.attach(&registry, "stale");
        assert!(client.rebind(&registry, "stale", "current"));
        let outcome = registry.publish("stale", &frame("old-id"));
        assert_eq!(outcome.delivered, 0);
        let outcome = registry.publish("current", &frame("new-id"));
        assert_eq!(outcome.delivered, 1);
        assert!(client.contains("current"));
        assert!(!client.contains("stale"));
        // The binding notice rides the new id's subscription.
        assert_eq!(drained(&mut rx).len(), 1);
        // A rebind of a connection that was never attached stays one.
        assert!(!client.rebind(&registry, "other", "fresh"));
    }

    #[tokio::test]
    async fn a_full_queue_drops_once_per_stall_cycle() {
        let registry = SessionSubscribers::new();
        let (tx, mut rx) = queue(2);
        let client = ClientSubscriptions::new("slow".into(), tx);
        client.attach(&registry, "session-1");
        // Fill the queue; every further publish drops, but the stall
        // cycle reports once (the log cadence the ring's Lagged had).
        let mut lagged_lines = 0;
        for index in 0..5 {
            let outcome = registry.publish("session-1", &frame(&format!("f{index}")));
            lagged_lines += outcome.lagged.len();
        }
        assert_eq!(lagged_lines, 1);
        // The frames that fit arrive in publish order.
        let frames = drained(&mut rx);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["type"], "f0");
        assert_eq!(frames[1]["type"], "f1");
        // A drained queue resets the cycle: the next stall reports again.
        let outcome = registry.publish("session-1", &frame("f5"));
        assert_eq!(outcome.delivered, 1);
        let outcome = registry.publish("session-1", &frame("f6"));
        assert!(outcome.lagged.is_empty());
        let outcome = registry.publish("session-1", &frame("f7"));
        assert_eq!(outcome.lagged.len(), 1);
    }

    /// Upstream #2444/#1940/#1901: a connection whose queue dropped frames (a lost
    /// `tool_execution_end`/`agent_end` among them) gets one resync frame per lagged
    /// session once its queue drains, and its writer is woken for it.
    #[tokio::test]
    async fn a_dropped_frame_queues_one_resync_after_the_drain() {
        let registry = SessionSubscribers::new();
        let (tx, mut rx) = queue(2);
        let client = ClientSubscriptions::new("slow".into(), tx);
        client.attach(&registry, "session-1");
        client.attach(&registry, "session-2");
        for tag in ["f0", "f1", "agent_end", "agent_end"] {
            registry.publish("session-1", &frame(tag));
        }
        // The writer was woken by the drop (a stored permit).
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.lagged().notified(),
        )
        .await
        .expect("the lag mark wakes the writer");
        let delivered = drained(&mut rx);
        client.queue_pending_resyncs();
        // One resync, for the lagged session only; the mark is consumed.
        let resyncs = drained(&mut rx);
        client.queue_pending_resyncs();
        assert_eq!(
            (delivered, resyncs, drained(&mut rx)),
            (
                vec![
                    serde_json::json!({ "type": "f0" }),
                    serde_json::json!({ "type": "f1" })
                ],
                vec![serde_json::json!({
                    "type": "session_resync_required",
                    "activeSessionId": "session-1",
                    "reason": "lagged",
                })],
                Vec::new(),
            )
        );
    }

    /// A session detached before the drain needs no resync.
    #[tokio::test]
    async fn a_detached_session_gets_no_resync() {
        let registry = SessionSubscribers::new();
        let (tx, mut rx) = queue(1);
        let client = ClientSubscriptions::new("slow".into(), tx);
        client.attach(&registry, "session-1");
        registry.publish("session-1", &frame("f0"));
        registry.publish("session-1", &frame("dropped"));
        client.detach(&registry, "session-1");
        drained(&mut rx);
        client.queue_pending_resyncs();
        assert_eq!(drained(&mut rx), Vec::<Value>::new());
    }

    #[tokio::test]
    async fn a_closed_queue_prunes_its_subscription() {
        let registry = SessionSubscribers::new();
        let (tx, rx) = queue(2);
        let client = ClientSubscriptions::new("gone".into(), tx);
        client.attach(&registry, "session-1");
        drop(rx);
        let outcome = registry.publish("session-1", &frame("after-close"));
        assert_eq!(outcome.delivered, 0);
        // The prune freed the session entry: the list stays consistent for
        // disconnect cleanup.
        client.detach_all(&registry);
        let outcome = registry.publish("session-1", &frame("after-prune"));
        assert_eq!(outcome.delivered, 0);
    }

    #[tokio::test]
    async fn attach_is_idempotent_and_disconnect_clears_every_session() {
        let registry = SessionSubscribers::new();
        let (tx, mut rx) = queue(8);
        let client = ClientSubscriptions::new("client".into(), tx);
        client.attach(&registry, "a");
        client.attach(&registry, "a");
        client.attach(&registry, "b");
        let outcome = registry.publish("a", &frame("one"));
        assert_eq!(
            outcome.delivered, 1,
            "a duplicate attach must not double-deliver"
        );
        assert_eq!(drained(&mut rx).len(), 1);
        client.detach_all(&registry);
        assert_eq!(registry.publish("b", &frame("late")).delivered, 0);
        assert!(drained(&mut rx).is_empty());
    }
}
