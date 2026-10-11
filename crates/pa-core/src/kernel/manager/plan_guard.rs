//! Plan mode's in-kernel fallback: the out-of-band `plan_guard` frame that
//! arms or disarms the runtime's write guard where no OS sandbox can enforce
//! plan mode (see `crate::kernel::plan_guard`).

use super::{Arc, Duration, Inner, ReplKernelManager, Value, anyhow, json, lock, oneshot};
use crate::kernel::plan_guard::{PLAN_GUARD_SETTLE_TIMEOUT_MS, mint_plan_guard_token};

impl Inner {
    /// Send the session's current plan-mode state to the running kernel and
    /// wait for the runtime's answer. Serialized, and the switch is read under
    /// the lock, so the last frame the runtime sees carries the latest state.
    /// `Ok(None)` when this manager has no plan guard configured.
    pub(super) async fn apply_plan_guard(self: &Arc<Self>) -> anyhow::Result<Option<bool>> {
        let Some(guard) = &self.options.plan_guard else {
            return Ok(None);
        };
        let _serialized = self.plan_guard_lock.lock().await;
        let enabled = guard.mode.is_enabled();
        let token = {
            let mut token = lock(&self.plan_guard_token);
            if token.is_none() {
                *token = Some(mint_plan_guard_token()?);
            }
            token.clone().unwrap_or_default()
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        lock(&self.guarded)
            .plan_guard_waiters
            .insert(request_id.clone(), tx);
        let paths = |roots: &[std::path::PathBuf]| -> Vec<String> {
            roots
                .iter()
                .map(|root| root.to_string_lossy().into_owned())
                .collect()
        };
        let frame = json!({
            "type": "plan_guard",
            "id": request_id,
            "token": token,
            "enabled": enabled,
            "writable_roots": paths(&guard.writable_roots),
            "protected_roots": paths(&guard.protected_roots),
        });
        if let Err(error) = self.write_line(&frame).await {
            lock(&self.guarded).plan_guard_waiters.remove(&request_id);
            return Err(error);
        }
        let settled = tokio::time::timeout(Duration::from_millis(PLAN_GUARD_SETTLE_TIMEOUT_MS), rx)
            .await
            .ok()
            .and_then(Result::ok);
        let Some(fields) = settled else {
            lock(&self.guarded).plan_guard_waiters.remove(&request_id);
            return Err(anyhow!(
                "the kernel runtime did not answer the plan-mode guard request \
                 (prime-agent-runtime predates plan mode?)"
            ));
        };
        if fields.get("status").and_then(Value::as_str) != Some("ok") {
            return Err(anyhow!(
                "the kernel runtime refused the plan-mode guard request: {}",
                fields
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown reason")
            ));
        }
        let armed = fields.get("enabled").and_then(Value::as_bool);
        if armed != Some(enabled) {
            return Err(anyhow!(
                "the kernel runtime reported plan mode {armed:?} after a request for {enabled}"
            ));
        }
        // The guard cannot confine a process, and this kernel has no OS
        // sandbox: the host refuses its `bash()` jobs while the guard is armed
        // (the runtime refuses them first; this holds against a cell that
        // sends the host request itself).
        if self.options.sandbox.is_none() {
            self.bash_jobs.set_sandbox(if enabled {
                pa_bash::JobSandbox::Unavailable(guard.job_refusal())
            } else {
                pa_bash::JobSandbox::Unconfined
            });
        }
        Ok(Some(enabled))
    }
}

impl ReplKernelManager {
    /// Re-send the plan guard's current state to a running kernel (a plan
    /// mode toggle). A kernel that is not running picks the state up when it
    /// starts; `Ok(None)` then, and when no guard is configured.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime does not answer, refuses the frame,
    /// or reports a state other than the requested one.
    pub async fn sync_plan_guard(&self) -> anyhow::Result<Option<bool>> {
        if !self.is_running() {
            return Ok(None);
        }
        self.inner.apply_plan_guard().await
    }
}
