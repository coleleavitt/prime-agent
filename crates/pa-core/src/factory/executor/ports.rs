//! The executor's seams: the child-session host it admits children
//! through, the parent-notice lane milestones ride, and the clock budgets
//! and backoff measure against.
//!
//! Boxed futures (not RPITIT): the executor holds every seam as a `dyn`
//! object, so a production adapter, a test double, and a future host can
//! all plug in without making the executor generic.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::session_engine::rlm_host::{RlmChildResult, RlmHostBridge};

/// One pending seam call.
pub type PortFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// One spawn admission the executor asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactorySpawn {
    pub prompt: String,
    pub name: String,
    pub model: Option<String>,
    pub thinking: Option<String>,
}

/// The child-session host: admission, settlement, cancellation.
///
/// Implementations answer errors as their user-facing sentence (the
/// executor classifies rate limits by text and records the sentence in the
/// ledger); `collect` returns snapshots on timeout, never an error, and a
/// deleted child vanishes from later collect results.
pub trait FactoryChildren: Send + Sync {
    /// Admit one child; answers its child id.
    fn spawn(&self, request: FactorySpawn) -> PortFuture<Result<String, String>>;
    /// Settle snapshots for the named children, waiting up to `timeout_ms`.
    fn collect(
        &self,
        child_ids: Vec<String>,
        timeout_ms: u64,
    ) -> PortFuture<Result<Vec<RlmChildResult>, String>>;
    /// Cancel and remove one child.
    fn delete(&self, child_id: String) -> PortFuture<Result<(), String>>;
}

/// The parent-notice lane: one quiet notice per milestone kind per run.
///
/// An implementation that cannot deliver answers an error; the milestone
/// stays in the ledger (stage `recorded`) and `status()` still surfaces it.
pub trait FactoryNotices: Send + Sync {
    fn notify(&self, payload: Value) -> PortFuture<Result<(), String>>;
}

/// The executor's clock: seconds on one monotonic-enough axis, and a sleep
/// on the same axis (tests inject a fake pair).
pub trait FactoryClock: Send + Sync {
    fn now(&self) -> f64;
    fn sleep(&self, seconds: f64) -> PortFuture<()>;
}

/// Wall-clock seconds since the epoch: a persisted run record's times stay
/// meaningful across a host restart.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl FactoryClock for SystemClock {
    fn now(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_secs_f64())
    }

    fn sleep(&self, seconds: f64) -> PortFuture<()> {
        let duration = std::time::Duration::from_secs_f64(seconds.max(0.0));
        Box::pin(tokio::time::sleep(duration))
    }
}

/// The session's own child host behind the kernel `rlm.*` surface: factory
/// children ride exactly the `rlm.spawn` admission path (payload
/// validation, plan mode, delegation budget, usage attribution, the daemon
/// allowlist pin), so a factory child is an ordinary child.
pub struct SessionChildren {
    bridge: Arc<RlmHostBridge>,
}

impl SessionChildren {
    #[must_use]
    pub fn new(bridge: Arc<RlmHostBridge>) -> Self {
        Self { bridge }
    }
}

impl FactoryChildren for SessionChildren {
    fn spawn(&self, request: FactorySpawn) -> PortFuture<Result<String, String>> {
        let bridge = Arc::clone(&self.bridge);
        Box::pin(async move {
            let mut kwargs = serde_json::Map::new();
            kwargs.insert("name".into(), Value::from(request.name));
            if let Some(model) = request.model {
                kwargs.insert("model".into(), Value::from(model));
            }
            if let Some(thinking) = request.thinking {
                kwargs.insert("thinking".into(), Value::from(thinking));
            }
            let payload = json!({ "prompt": request.prompt, "kwargs": kwargs });
            let handle = bridge
                .spawn_from_payload(&payload, None)
                .await
                .map_err(|error| format!("{error:#}"))?;
            handle
                .get("rlm_child_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "rlm.spawn returned no child id".to_string())
        })
    }

    fn collect(
        &self,
        child_ids: Vec<String>,
        timeout_ms: u64,
    ) -> PortFuture<Result<Vec<RlmChildResult>, String>> {
        let host = self.bridge.child_host();
        Box::pin(async move {
            host.collect(child_ids, timeout_ms)
                .await
                .map_err(|error| format!("{error:#}"))
        })
    }

    fn delete(&self, child_id: String) -> PortFuture<Result<(), String>> {
        let host = self.bridge.child_host();
        Box::pin(async move {
            host.delete_subagent(child_id)
                .await
                .map(|_| ())
                .map_err(|error| format!("{error:#}"))
        })
    }
}

/// No parent-notice lane: the daemon's factory notice injection is not
/// ported yet (the TS-era `factory.progress` lane), so milestones stay in
/// the ledger exactly as they did when the kernel's notice request found
/// no host handler.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoNoticeLane;

impl FactoryNotices for NoNoticeLane {
    fn notify(&self, _payload: Value) -> PortFuture<Result<(), String>> {
        Box::pin(async { Err("the factory notice lane is not available".to_string()) })
    }
}
