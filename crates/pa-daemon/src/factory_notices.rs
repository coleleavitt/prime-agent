//! The factory run notice lane: the session's factory executor (pa-core)
//! announces each run milestone — completed, failed, paused, interrupted by
//! a host restart, cancelled by the user — through the
//! [`FactoryNotices`] port, and the worker that owns the session queues it
//! as a `<task-notification>` row on the follow-up lane (busy sessions read
//! it after their run, idle ones wake into it). A kernel
//! `factory.status`/`watch`/`stop` that already showed the model the run's
//! state withdraws the still-queued notice for that state.

use std::sync::Arc;

use pa_core::factory::executor::ports::{FactoryNotice, FactoryNotices, PortFuture};
use pa_types::sync::MutexExt;

use crate::agent_engine::AgentSessionEngine;

/// Queues one notice row; answers whether it was admitted (a closed
/// session admits nothing).
pub type FactoryNoticeAdmit = Arc<dyn Fn(&pa_types::session::CustomMessage) -> bool + Send + Sync>;

/// Withdraws the queued notices for `(run_id, run_state)`.
pub type FactoryNoticeWithdraw = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// The worker's [`FactoryNotices`] implementation over its queue seams.
pub struct WorkerFactoryNotices {
    admit: FactoryNoticeAdmit,
    withdraw: FactoryNoticeWithdraw,
}

impl WorkerFactoryNotices {
    #[must_use]
    pub fn new(admit: FactoryNoticeAdmit, withdraw: FactoryNoticeWithdraw) -> Self {
        Self { admit, withdraw }
    }
}

impl FactoryNotices for WorkerFactoryNotices {
    fn notify(&self, notice: FactoryNotice) -> PortFuture<Result<(), String>> {
        let admitted = (self.admit)(&notice.message(crate::util::now_ms()));
        Box::pin(async move {
            if admitted {
                Ok(())
            } else {
                Err("the session is closed".to_string())
            }
        })
    }

    fn withdraw(&self, run_id: &str, run_state: &str) {
        (self.withdraw)(run_id, run_state);
    }
}

impl AgentSessionEngine {
    /// Wire the worker's factory notice lane (called once at construction,
    /// before the first session build reads it).
    pub fn set_factory_notices(&self, notices: Arc<dyn FactoryNotices>) {
        *self.factory_notices.lock_or_recover() = Some(notices);
    }
}
