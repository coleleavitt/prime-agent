//! Billed model calls made for a session from outside its engine: a
//! feature's (the observer's digest reads, the advisor's answers, a RAVO
//! run's prompts) and the side model's (the goal evaluator, titles, recaps,
//! tool-use labels). The engine registers the session's durable usage store
//! when it builds the session, and each such call lands there as a
//! `model_usage` row, the way the engine's own refinement and side-question
//! calls do ([`super::AgentSession::record_model_usage`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use super::model_usage::ModelUsage;
use super::rlm_usage::RlmChildUsageStore;

type UsageStores = Mutex<HashMap<String, Weak<dyn RlmChildUsageStore>>>;

fn usage_stores() -> &'static UsageStores {
    static STORES: OnceLock<UsageStores> = OnceLock::new();
    STORES.get_or_init(UsageStores::default)
}

/// Route the calls billed for session `session_id` to `store` while the
/// engine keeps it alive (the registry holds it weakly); replaces an
/// earlier store.
pub fn register_session_usage_store(session_id: &str, store: &Arc<dyn RlmChildUsageStore>) {
    let mut stores = usage_stores()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    stores.retain(|_, store| store.strong_count() > 0);
    stores.insert(session_id.to_string(), Arc::downgrade(store));
}

/// Record one call billed for session `session_id` as a `model_usage` row
/// in its durable store. A session without a live store (it ended, or was
/// never built in this process) drops the bill; a failed append is logged.
/// Once handed off, the append survives cancellation of this caller.
pub async fn record_session_model_usage(session_id: &str, usage: Option<ModelUsage>) {
    let Some(usage) = usage else {
        return;
    };
    let store = usage_stores()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(session_id)
        .and_then(Weak::upgrade);
    let Some(store) = store else {
        return;
    };
    let session_id = session_id.to_string();
    let recording = tokio::spawn(async move {
        if let Err(error) = store.append_model_usage(session_id, usage).await {
            eprintln!("pa-core: model usage row not persisted: {error}");
        }
    });
    if let Err(error) = recording.await {
        eprintln!("pa-core: model usage recording task failed: {error}");
    }
}

/// A store that keeps every bill it is given.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingStore(pub(crate) Mutex<Vec<(String, ModelUsage)>>);

#[cfg(test)]
impl RlmChildUsageStore for RecordingStore {
    fn last_assistant(
        &self,
    ) -> super::rlm_usage::RlmChildUsageFuture<'_, Option<(String, pa_types::ai::Usage)>> {
        Box::pin(async { None })
    }

    fn append_attribution(
        &self,
        _row_id: &str,
        _target_id: &str,
        _child_usage: pa_types::ai::Usage,
        _origin: Option<pa_types::session::ChildUsageOrigin>,
    ) -> super::rlm_usage::RlmChildUsageFuture<
        '_,
        std::io::Result<super::rlm_usage::ChildUsageAppendResult>,
    > {
        Box::pin(async { Err(std::io::Error::other("bills only")) })
    }

    fn append_model_usage(
        &self,
        session_id: String,
        usage: ModelUsage,
    ) -> super::rlm_usage::RlmChildUsageFuture<'_, std::io::Result<()>> {
        self.0.lock().unwrap().push((session_id, usage));
        Box::pin(async { Ok(()) })
    }

    fn observe_billed_rows(
        &self,
        _sink: crate::session::billed_row::BilledRowSink,
    ) -> super::rlm_usage::RlmChildUsageFuture<'_, ()> {
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bill(total_tokens: u64) -> ModelUsage {
        ModelUsage::new(
            "observer",
            "p",
            "m",
            pa_types::ai::Usage {
                input: total_tokens,
                total_tokens,
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// A bill lands in its own session's live store only: another session's
    /// store, and a session whose store was dropped, get nothing.
    #[tokio::test]
    async fn a_bill_lands_in_its_sessions_live_store_only() {
        let first = Arc::new(RecordingStore::default());
        let second = Arc::new(RecordingStore::default());
        let first_store: Arc<dyn RlmChildUsageStore> = first.clone();
        let second_store: Arc<dyn RlmChildUsageStore> = second.clone();
        register_session_usage_store("billed-calls-first", &first_store);
        register_session_usage_store("billed-calls-second", &second_store);
        record_session_model_usage("billed-calls-first", Some(bill(5))).await;
        record_session_model_usage("billed-calls-first", None).await;
        drop(second_store);
        drop(second);
        record_session_model_usage("billed-calls-second", Some(bill(7))).await;
        record_session_model_usage("billed-calls-unknown", Some(bill(9))).await;
        assert_eq!(
            *first.0.lock().unwrap(),
            vec![("billed-calls-first".to_string(), bill(5))]
        );
    }
}
