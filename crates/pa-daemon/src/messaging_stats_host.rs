//! The kernel host side of the session messaging counters (upstream #2352):
//! `rlm.messaging_stats` and the outbound-send counting around
//! `agent_message.send`.
//!
//! The receiving WORKER owns the counters (the digest lane records arrivals
//! and model steps where it already sees them, and reads the same counters
//! for its controller), so it installs closures into the engine
//! ([`AgentSessionEngine::set_messaging_stats_seams`]) at construction; the
//! engine registers the handlers only when the seams exist — anything
//! without the worker leaves the request honestly unavailable.

use pa_types::sync::MutexExt;
use std::sync::Arc;

use serde_json::Value;

use pa_core::kernel::shared::{host_handler, HostRequestHandlers};

use crate::agent_engine::AgentSessionEngine;

/// The worker-installed messaging-counter seams.
#[derive(Clone)]
pub struct MessagingStatsSeams {
    /// The `rlm.messaging_stats()` snapshot (the TS wire shape).
    pub snapshot: Arc<dyn Fn() -> pa_core::swarm_eval::MessagingStatsSnapshot + Send + Sync>,
    /// One resolved `agent_message.send` attempt (`true` when it failed).
    pub record_send: Arc<dyn Fn(bool) + Send + Sync>,
}

impl AgentSessionEngine {
    /// Install the messaging-counter seams (the worker calls this at
    /// construction, before the first session build registers the handlers).
    pub fn set_messaging_stats_seams(&self, seams: MessagingStatsSeams) {
        *self.messaging_stats_seams.lock_or_recover() = Some(seams);
    }

    /// `rlm.messaging_stats` plus the send counting: the already-registered
    /// `agent_message.send` handler is wrapped so every attempt that
    /// reaches the delivery (a string `message`; TS validates that before
    /// counting) counts at resolution, a rejection as a failure.
    pub(crate) fn register_messaging_stats_host_handlers(
        &self,
        handlers: &mut HostRequestHandlers,
    ) {
        let Some(seams) = self.messaging_stats_seams.lock_or_recover().clone() else {
            return;
        };
        if let Some(send) = handlers.get("agent_message.send").cloned() {
            let record_send = Arc::clone(&seams.record_send);
            handlers.register(
                "agent_message.send",
                host_handler(move |payload| {
                    let send = Arc::clone(&send);
                    let record_send = Arc::clone(&record_send);
                    Box::pin(async move {
                        let counted = payload.data.get("message").is_some_and(Value::is_string);
                        let result = send(payload).await;
                        if counted {
                            record_send(result.is_err());
                        }
                        result
                    })
                }),
            );
        }
        let snapshot = Arc::clone(&seams.snapshot);
        let telemetry = Arc::clone(&self.session_telemetry);
        handlers.register(
            "rlm.messaging_stats",
            host_handler(move |_payload| {
                let snapshot = Arc::clone(&snapshot);
                let telemetry = telemetry.lock_or_recover().clone();
                Box::pin(async move {
                    if let Some(telemetry) = telemetry {
                        telemetry.note_adoption(
                            pa_core::session_engine::telemetry::SessionAdoption::MessagingStatsRead,
                        );
                    }
                    Ok(serde_json::to_value(snapshot())?)
                })
            }),
        );
    }
}
