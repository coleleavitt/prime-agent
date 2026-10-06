//! The agent-message ingestion surface: the worker arms for
//! `agent_messages_status`/`_pause`/`_resume`/`_clear`, plus the paused
//! delivery gate. Deliberate deviation from TS: no per-sender rate bucket
//! (the daemon's queue capacity bound is the enforced limit).

use pa_types::sync::MutexExt;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};

use pa_core::session_engine::agent_messaging::{
    DEFAULT_AGENT_MESSAGE_MAX_CHARS, DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION,
    DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY, DEFAULT_AGENT_MESSAGE_RATE_LIMIT_REFILL_MS,
};

use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::{QueuedItem, Worker};

/// The body prefix of a drop notice (upstream #2329): a dropped notice is
/// never itself reported, so two sessions can never trade notices.
const DROP_NOTICE_PREFIX: &str = "[agent-message-failed]";

/// The supervisor round-trip budget of one drop notice (a closing
/// session awaits its notices, so the bound keeps the close prompt).
const DROP_NOTICE_TIMEOUT_MS: u64 = 2_000;

/// Why queued agent messages were dropped before delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentMessageDropReason {
    Cleared,
    Paused,
    Closed,
}

impl AgentMessageDropReason {
    fn describe(self) -> &'static str {
        match self {
            AgentMessageDropReason::Cleared => "the target cleared its queued agent messages",
            AgentMessageDropReason::Paused => "the target paused agent messaging",
            AgentMessageDropReason::Closed => "the target session closed",
        }
    }
}

/// One dropped agent message an agent sender can be told about: the
/// sender's live session and the message id its receipt carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DroppedAgentMessage {
    sender_active_session_id: String,
    message_id: String,
}

impl DroppedAgentMessage {
    /// The sender route of a queued agent-message item; `None` for client
    /// prompts, CLI senders (no live session to notify), and drop notices.
    pub(crate) fn of(item: &QueuedItem) -> Option<Self> {
        if item
            .agent_message
            .as_deref()?
            .starts_with(DROP_NOTICE_PREFIX)
        {
            return None;
        }
        let details = item.custom_message.as_ref()?.get("details")?;
        let sender = details
            .get("from")?
            .get("activeSessionId")?
            .as_str()
            .filter(|id| !id.is_empty())?;
        let message_id = details.get("id")?.as_str()?;
        Some(DroppedAgentMessage {
            sender_active_session_id: sender.to_string(),
            message_id: message_id.to_string(),
        })
    }
}

/// Where a worker's drop notices go: the supervisor routes each one to
/// its sender as an agent message from this session.
pub(crate) struct DropNoticeRoute {
    socket: std::path::PathBuf,
    from_active_session_id: String,
}

impl DropNoticeRoute {
    /// Tell each agent sender whose queued messages were dropped (upstream
    /// #2329): one notice per sender, so a busy sender sees it at its next
    /// boundary and an idle one wakes. Returns once every notice was handed
    /// to the supervisor (or failed); a failure is logged, never retried.
    pub(crate) async fn notify(
        self,
        dropped: Vec<DroppedAgentMessage>,
        reason: AgentMessageDropReason,
    ) {
        let mut by_sender: Vec<(String, Vec<String>)> = Vec::new();
        for message in dropped {
            if message.sender_active_session_id == self.from_active_session_id {
                continue;
            }
            match by_sender
                .iter_mut()
                .find(|(sender, _)| *sender == message.sender_active_session_id)
            {
                Some((_, ids)) => ids.push(message.message_id),
                None => {
                    by_sender.push((message.sender_active_session_id, vec![message.message_id]));
                }
            }
        }
        let link = crate::supervisor_link::SupervisorLink::new(self.socket);
        for (sender, ids) in by_sender {
            let sent = link
                .request_success(
                    json!({
                        "type": "send_message",
                        "targetActiveSessionId": sender,
                        "message": drop_notice_text(&ids, reason),
                        "fromActiveSessionId": self.from_active_session_id,
                        "agentOrigin": true,
                    }),
                    std::time::Duration::from_millis(DROP_NOTICE_TIMEOUT_MS),
                )
                .await;
            if let Err(error) = sent {
                eprintln!("pa-daemon: agent-message drop notice to {sender} failed: {error:#}");
            }
        }
    }
}

/// The notice body one sender receives for its dropped messages.
fn drop_notice_text(message_ids: &[String], reason: AgentMessageDropReason) -> String {
    let count = message_ids.len();
    let plural = if count == 1 { "" } else { "s" };
    format!(
        "{DROP_NOTICE_PREFIX} {count} agent message{plural} you sent here {verb} dropped before delivery: {}. Dropped: {}. {it} never reached this session's context; resend if still needed.",
        reason.describe(),
        message_ids.join(", "),
        verb = if count == 1 { "was" } else { "were" },
        it = if count == 1 { "It" } else { "They" },
    )
}

/// The worker's agent-message ingestion state: the pause flag.
pub(crate) struct AgentMessageIngest {
    paused: AtomicBool,
}

impl AgentMessageIngest {
    pub(crate) fn new() -> Self {
        AgentMessageIngest {
            paused: AtomicBool::new(false),
        }
    }

    pub(crate) fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }
}

impl Default for AgentMessageIngest {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker {
    /// The safety-status wire object (see the module note for the rate-limiter
    /// deviation).
    fn agent_message_safety_status(&self) -> Value {
        json!({
            "paused": self.agent_messages.paused(),
            "maxMessageChars": DEFAULT_AGENT_MESSAGE_MAX_CHARS,
            "maxPendingPerSession": DEFAULT_AGENT_MESSAGE_MAX_PENDING_PER_SESSION,
            "rateLimitCapacity": DEFAULT_AGENT_MESSAGE_RATE_LIMIT_CAPACITY,
            "rateLimitRefillMs": DEFAULT_AGENT_MESSAGE_RATE_LIMIT_REFILL_MS,
        })
    }

    /// `agent_messages_status`: the safety status, no side effects.
    pub(crate) fn handle_agent_messages_status(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("agent_messages_status") {
            return response;
        }
        response_success(
            None,
            "agent_messages_status",
            Some(self.agent_message_safety_status()),
        )
    }

    /// `agent_messages_pause`: set the flag, drop every queued agent-message item, answer the
    /// safety status.
    pub(crate) fn handle_agent_messages_pause(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("agent_messages_pause") {
            return response;
        }
        self.agent_messages.set_paused(true);
        let (_, dropped) = self.clear_queued_agent_messages();
        self.spawn_drop_notices(dropped, AgentMessageDropReason::Paused);
        response_success(
            None,
            "agent_messages_pause",
            Some(self.agent_message_safety_status()),
        )
    }

    /// `agent_messages_resume`: clear the flag, answer the safety status.
    pub(crate) fn handle_agent_messages_resume(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("agent_messages_resume") {
            return response;
        }
        self.agent_messages.set_paused(false);
        response_success(
            None,
            "agent_messages_resume",
            Some(self.agent_message_safety_status()),
        )
    }

    /// `agent_messages_clear`: drop queued agent-message items, answer the TS shape (removed
    /// prompts per lane).
    pub(crate) fn handle_agent_messages_clear(&self) -> DaemonResponse {
        if let Err(response) = self.require_created("agent_messages_clear") {
            return response;
        }
        let (cleared, dropped) = self.clear_queued_agent_messages();
        self.spawn_drop_notices(dropped, AgentMessageDropReason::Cleared);
        response_success(None, "agent_messages_clear", Some(cleared))
    }

    /// The delivery gate's paused check: the exact TS error string, surfaced
    /// through `worker_deliver_message`.
    #[allow(clippy::result_large_err)]
    pub(crate) fn refuse_delivery_if_paused(&self) -> Result<(), DaemonResponse> {
        if self.agent_messages.paused() {
            return Err(response_failure(
                None,
                "worker_deliver_message",
                "Agent messaging is paused",
                None,
            ));
        }
        Ok(())
    }

    /// The drop-notice route for this worker: the supervisor socket and
    /// this session's live id (`None` without a supervisor link).
    pub(crate) fn drop_notice_route(&self) -> Option<DropNoticeRoute> {
        let socket = self.config.supervisor_socket_path.clone();
        (!socket.as_os_str().is_empty()).then(|| DropNoticeRoute {
            socket,
            from_active_session_id: self.config.active_session_id.clone(),
        })
    }

    /// The fire-and-forget form for command arms: the reply never waits on
    /// the notices.
    fn spawn_drop_notices(
        &self,
        dropped: Vec<DroppedAgentMessage>,
        reason: AgentMessageDropReason,
    ) {
        if dropped.is_empty() {
            return;
        }
        if let Some(route) = self.drop_notice_route() {
            tokio::spawn(route.notify(dropped, reason));
        }
    }

    /// Remove the queued agent-message items from both lanes (never
    /// client-queued prompts), in the `{ steering, followUp }` shape, plus
    /// the dropped messages' sender routes.
    fn clear_queued_agent_messages(&self) -> (Value, Vec<DroppedAgentMessage>) {
        let mut core = self.core.lock_or_recover();
        let mut steering = Vec::new();
        let mut follow_up = Vec::new();
        let mut dropped = Vec::new();
        let mut retained_steering = std::collections::VecDeque::new();
        while let Some(item) = core.steering.pop_front() {
            match item.agent_message {
                Some(_) => {
                    dropped.extend(DroppedAgentMessage::of(&item));
                    steering.push(item.message);
                }
                None => retained_steering.push_back(item),
            }
        }
        core.steering = retained_steering;
        let mut retained_follow_up = std::collections::VecDeque::new();
        while let Some(item) = core.follow_up.pop_front() {
            match item.agent_message {
                Some(_) => {
                    dropped.extend(DroppedAgentMessage::of(&item));
                    follow_up.push(item.message);
                }
                None => retained_follow_up.push_back(item),
            }
        }
        core.follow_up = retained_follow_up;
        let snapshot = Self::snapshot_locked(&core);
        drop(core);
        // The sweep may have settled the session: the verdict follows the remaining lanes.
        self.checkpoint_queue(crate::worker::QueueCheckpoint::Settle {
            operation: "queue_mutated",
        });
        let _ = self.emit_action_update(&snapshot);
        (
            json!({ "steering": steering, "followUp": follow_up }),
            dropped,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    async fn created_worker() -> Arc<Worker> {
        let dir = std::env::temp_dir().join(format!("pa-worker-ami-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = crate::worker::WorkerConfig {
            socket_path: dir.join("worker.sock"),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "ami-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        let worker = Arc::new(Worker::new(config, None));
        let created = worker
            .dispatch(
                "create",
                &json!({ "noSession": true, "cwd": "/tmp", "name": "ami" }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        worker
    }

    /// A pause clears queued agent messages but keeps client prompts.
    #[tokio::test]
    async fn status_pause_resume_and_clear_match_ts_shapes() {
        let worker = created_worker().await;

        let status = worker.dispatch("agent_messages_status", &json!({})).await;
        assert!(status.success);
        assert_eq!(
            status.data,
            Some(json!({
                "paused": false,
                "maxMessageChars": 16384,
                "maxPendingPerSession": 20,
                "rateLimitCapacity": 3,
                "rateLimitRefillMs": 1000,
            }))
        );

        // One agent-message delivery and one client steer, both on the steering lane.
        let delivered = worker
            .dispatch(
                "worker_deliver_message",
                &json!({
                    "targetActiveSessionId": "ami-session",
                    "message": "from a peer",
                    "sender": { "activeSessionId": "peer-1" },
                }),
            )
            .await;
        assert!(delivered.success, "delivery failed: {delivered:?}");
        worker
            .dispatch(
                "steer",
                &json!({ "activeSessionId": "ami-session", "message": "client text" }),
            )
            .await;

        // Pause: the flag flips and the queued agent message is dropped; the client prompt stays.
        let paused = worker.dispatch("agent_messages_pause", &json!({})).await;
        assert!(paused.success);
        assert_eq!(paused.data.as_ref().unwrap()["paused"], json!(true));
        {
            let core = worker.core.lock().unwrap();
            let texts: Vec<&str> = core
                .steering
                .iter()
                .map(|item| item.message.as_str())
                .collect();
            assert_eq!(texts, vec!["client text"]);
        }

        // A delivery while paused answers the TS gate error.
        let refused = worker
            .dispatch(
                "worker_deliver_message",
                &json!({
                    "targetActiveSessionId": "ami-session",
                    "message": "while paused",
                    "sender": { "activeSessionId": "peer-1" },
                }),
            )
            .await;
        assert!(!refused.success);
        assert_eq!(refused.error.as_deref(), Some("Agent messaging is paused"));

        // Resume flips the flag back.
        let resumed = worker.dispatch("agent_messages_resume", &json!({})).await;
        assert_eq!(resumed.data.as_ref().unwrap()["paused"], json!(false));

        // Clear answers the TS removed-prompts shape; the client prompt stays.
        let cleared = worker.dispatch("agent_messages_clear", &json!({})).await;
        assert!(cleared.success);
        assert_eq!(
            cleared.data,
            Some(json!({ "steering": [], "followUp": [] }))
        );
        {
            let core = worker.core.lock().unwrap();
            let texts: Vec<&str> = core
                .steering
                .iter()
                .map(|item| item.message.as_str())
                .collect();
            assert_eq!(texts, vec!["client text"]);
        }
    }

    #[tokio::test]
    async fn clear_reports_the_follow_up_lane() {
        let worker = created_worker().await;
        let delivered = worker
            .dispatch(
                "worker_deliver_message",
                &json!({
                    "targetActiveSessionId": "ami-session",
                    "message": "queued for later",
                    "sender": { "activeSessionId": "peer-1" },
                    "deliveryMode": "follow_up",
                }),
            )
            .await;
        assert!(delivered.success, "delivery failed: {delivered:?}");
        let cleared = worker.dispatch("agent_messages_clear", &json!({})).await;
        // The removed text is the queued turn's prompt (TS `payload.text`).
        let mut cleared = cleared.data.expect("cleared data");
        for text in cleared["followUp"].as_array_mut().into_iter().flatten() {
            *text = json!(crate::worker::without_sent_stamp(
                text.as_str().unwrap_or_default()
            ));
        }
        assert_eq!(
            cleared,
            json!({
                "steering": [],
                "followUp": ["[agent-message from peer-1]\n\nqueued for later"],
            })
        );
    }

    /// A scripted supervisor that records every command it receives and
    /// answers each with success.
    async fn recording_supervisor(
        socket: std::path::PathBuf,
    ) -> tokio::sync::mpsc::UnboundedReceiver<serde_json::Value> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = pa_types::platform::transport::bind_transport(&socket)
            .await
            .unwrap();
        tokio::spawn(async move {
            while let Ok(stream) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.split();
                    let mut reader = BufReader::new(reader);
                    writer
                        .write_all(
                            b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7}}\n",
                        )
                        .await
                        .unwrap();
                    let mut line = String::new();
                    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                        line.clear();
                        let id = value["id"].as_str().unwrap_or_default().to_string();
                        let command = value["command"].clone();
                        let kind = command["type"].as_str().unwrap_or_default().to_string();
                        let _ = tx.send(command);
                        let mut reply = serde_json::to_string(&crate::protocol::response_success(
                            Some(&id),
                            &kind,
                            Some(json!({})),
                        ))
                        .unwrap();
                        reply.push('\n');
                        if writer.write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        rx
    }

    /// Upstream #2329: a sender whose queued agent messages are dropped
    /// (here by a clear and by a pause) is told, with the message ids and the
    /// reason, through a supervisor-routed message from the target. A CLI
    /// sender (no live session) has no one to tell.
    #[tokio::test]
    async fn dropped_queued_messages_notify_their_agent_senders() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("sup.sock");
        let mut commands = recording_supervisor(socket.clone()).await;
        let config = crate::worker::WorkerConfig {
            socket_path: dir.path().join("worker.sock"),
            supervisor_socket_path: socket,
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "ami-session".to_string(),
            agent_dir: dir.path().join("agent"),
            recovery_journal_path: dir.path().join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": ["ack"] })),
        };
        let worker = Arc::new(Worker::new(config, None));
        let created = worker
            .dispatch(
                "create",
                &json!({ "noSession": true, "cwd": "/tmp", "name": "ami" }),
            )
            .await;
        assert!(created.success, "create failed: {created:?}");
        let deliver = |message: &'static str, sender: serde_json::Value| {
            let worker = Arc::clone(&worker);
            async move {
                let receipt = worker
                    .dispatch(
                        "worker_deliver_message",
                        &json!({
                            "targetActiveSessionId": "ami-session",
                            "message": message,
                            "sender": sender,
                        }),
                    )
                    .await;
                assert!(receipt.success, "delivery failed: {receipt:?}");
                receipt.data.unwrap()["id"].as_str().unwrap().to_string()
            }
        };
        let first = deliver("one", json!({ "activeSessionId": "peer-1" })).await;
        let second = deliver("two", json!({ "activeSessionId": "peer-1" })).await;
        deliver("from the cli", json!({ "clientId": "cli-1" })).await;
        assert!(
            worker
                .dispatch("agent_messages_clear", &json!({}))
                .await
                .success
        );
        let notice = tokio::time::timeout(std::time::Duration::from_secs(10), commands.recv())
            .await
            .expect("the clear notifies the sender")
            .unwrap();
        assert_eq!(
            notice,
            json!({
                "type": "send_message",
                "targetActiveSessionId": "peer-1",
                "message": format!(
                    "[agent-message-failed] 2 agent messages you sent here were dropped before delivery: the target cleared its queued agent messages. Dropped: {first}, {second}. They never reached this session's context; resend if still needed."
                ),
                "fromActiveSessionId": "ami-session",
                "agentOrigin": true,
            })
        );

        let third = deliver("three", json!({ "activeSessionId": "peer-2" })).await;
        assert!(
            worker
                .dispatch("agent_messages_pause", &json!({}))
                .await
                .success
        );
        let notice = tokio::time::timeout(std::time::Duration::from_secs(10), commands.recv())
            .await
            .expect("the pause notifies the sender")
            .unwrap();
        assert_eq!(
            (notice["targetActiveSessionId"].clone(), notice["message"].clone()),
            (
                json!("peer-2"),
                json!(format!(
                    "[agent-message-failed] 1 agent message you sent here was dropped before delivery: the target paused agent messaging. Dropped: {third}. It never reached this session's context; resend if still needed."
                ))
            )
        );
        assert!(
            commands.try_recv().is_err(),
            "the CLI sender is never notified"
        );

        // A killed target drops its queue for good: the close notifies too.
        assert!(
            worker
                .dispatch("agent_messages_resume", &json!({}))
                .await
                .success
        );
        let fourth = deliver("four", json!({ "activeSessionId": "peer-3" })).await;
        assert!(worker.dispatch("kill", &json!({})).await.success);
        let notice = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let command = commands.recv().await.unwrap();
                if command["type"] == "send_message" {
                    return command;
                }
            }
        })
        .await
        .expect("the close notifies the sender");
        assert_eq!(
            (notice["targetActiveSessionId"].clone(), notice["message"].clone()),
            (
                json!("peer-3"),
                json!(format!(
                    "[agent-message-failed] 1 agent message you sent here was dropped before delivery: the target session closed. Dropped: {fourth}. It never reached this session's context; resend if still needed."
                ))
            )
        );
    }
}
