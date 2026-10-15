//! Host request handling: execute-side requests answered by the host
//! (harness/goal/etc.) and their settle/exit waits.

use futures::FutureExt as _;

use super::{
    Arc,
    Duration,
    HostRequestPayload,
    Inner,
    MAX_HANDLED_HOST_REQUEST_IDS,
    Value,
    anyhow,
    json,
    lock,
};
use crate::kernel::shared::{HostHandlerFuture, with_host_request_cancellation};

impl Inner {
    /// Dispatch one typed request from kernel code to the registered handler
    /// and reply over the protocol. Unhandled requests answer with an error.
    pub(crate) fn start_host_request(self: &Arc<Self>, request_id: &str, data: &Value) {
        let cancellation = tokio_util::sync::CancellationToken::new();
        {
            let mut g = lock(&self.guarded);
            let (seen, order) = &mut g.handled_host_request_ids;
            if seen.contains(request_id) {
                return;
            }
            seen.insert(request_id.to_string());
            order.push_back(request_id.to_string());
            while seen.len() > MAX_HANDLED_HOST_REQUEST_IDS {
                if let Some(oldest) = order.pop_front() {
                    seen.remove(&oldest);
                } else {
                    break;
                }
            }
            g.host_request_cancellations
                .insert(request_id.to_string(), cancellation.clone());
        }
        let mut request: HostHandlerFuture = Box::pin(with_host_request_cancellation(
            cancellation,
            self.host_request_future(data),
        ));
        // Run the handler's synchronous prefix in line, before the reader
        // takes the next frame (TS `repl-manager.ts` dispatches through an
        // async IIFE, whose body runs up to its first await immediately).
        // The kernel orders frames on purpose: `bash.consumed` leaves ahead
        // of the reading cell's `done` so its notice is withdrawn before the
        // turn boundary probes the steering lane. Spawning the whole handler
        // let `done` overtake the withdrawal, the probe saw the stale notice
        // and ended the run, and the withdrawal then left the session idle
        // with nothing queued to resume it.
        let ready = (&mut request).now_or_never();
        let inner = Arc::clone(self);
        let request_id = request_id.to_string();
        let task = tokio::spawn(async move {
            let result = match ready {
                Some(result) => result,
                None => request.await,
            };
            let reply = match result {
                Ok(result) => json!({ "status": "ok", "result": result }),
                Err(error) => {
                    inner.append_diagnostic(&format!(
                        "host request failed for {request_id}: {error:#}"
                    ));
                    json!({ "status": "error", "error": format!("{error:#}") })
                }
            };
            // Settled: a late `host_cancel` for this id has nothing to cancel.
            lock(&inner.guarded)
                .host_request_cancellations
                .remove(&request_id);
            let frame = json!({ "type": "host_reply", "id": request_id, "data": reply });
            if let Err(error) = inner.write_line(&frame).await {
                inner.append_diagnostic(&format!(
                    "failed to send host request reply for {request_id}: {error:#}"
                ));
            }
        });
        let mut g = lock(&self.guarded);
        // Completed task handles are dropped so the inflight set stays bounded.
        g.host_inflight.retain(|handle| !handle.is_finished());
        g.host_inflight.push(task);
    }

    /// Resolve the handler for one request and start it: the returned future
    /// owns everything it needs, so the caller can poll it in line first.
    /// With a session gate set, the gate reviews the request first, the
    /// handler is resolved only once it allows it, and the gate observes the
    /// outcome before the reply is sent.
    fn host_request_future(&self, data: &Value) -> HostHandlerFuture {
        let handlers = &self.options.host_handlers;
        let request_type = data
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty());
        let (Some(gate), Some(request_type)) = (handlers.gate().cloned(), request_type) else {
            return dispatch_host_request(&self.bash_jobs, handlers, data);
        };
        let review = gate.review(request_type, data);
        let request_type = request_type.to_string();
        let bash_jobs = Arc::clone(&self.bash_jobs);
        let handlers = handlers.clone();
        let data = data.clone();
        Box::pin(async move {
            // A review may wait on the user; a `host_cancel` (the cell was
            // interrupted) stops the wait and refuses the request.
            let refusal = match crate::kernel::shared::host_request_cancellation() {
                Some(token) => tokio::select! {
                    biased;
                    refusal = review => refusal,
                    () = token.cancelled() => {
                        Some("the request was cancelled before it was allowed".to_string())
                    }
                },
                None => review.await,
            };
            if let Some(reason) = refusal {
                return Err(anyhow!("{request_type} refused: {reason}"));
            }
            let outcome = dispatch_host_request(&bash_jobs, &handlers, &data).await;
            let error = outcome
                .as_ref()
                .err()
                .map(|error| format!("{error:#}"))
                .unwrap_or_default();
            let observed = match &outcome {
                Ok(answer) => Ok(answer),
                Err(_) => Err(error.as_str()),
            };
            gate.after(&request_type, &data, observed).await;
            outcome
        })
    }
}

/// Resolve the handler for one request and start it.
fn dispatch_host_request(
    bash_jobs: &Arc<pa_bash::JobTable>,
    handlers: &crate::kernel::shared::HostRequestHandlers,
    data: &Value,
) -> HostHandlerFuture {
    let Some(obj) = data.as_object() else {
        return Box::pin(async { Err(anyhow!("host request payload must be an object")) });
    };
    let Some(request_type) = obj
        .get("type")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    else {
        return Box::pin(async { Err(anyhow!("host request payload must have a string type")) });
    };
    if pa_bash::REQUEST_TYPES.contains(&request_type) {
        // The kernel's bash() commands run here, on the kernel's behalf
        // (its cwd and environment ride on every request). The answer may
        // block (a run or a follow waits for the job's events), so it runs
        // on a blocking thread. A `host_cancel` (the kernel was
        // interrupted while it waited) ends a `bash.run`: its job is
        // killed and the run still answers, once the group is gone; the
        // other requests treat cancellation as advisory.
        let table = Arc::clone(bash_jobs);
        let data = data.clone();
        return Box::pin(async move {
            let cancel = pa_bash::RunCancel::default();
            let canceller = cancel.clone();
            let mut answer = tokio::task::spawn_blocking(move || {
                pa_bash::handle_cancellable(&table, &data, &canceller)
            });
            let token = crate::kernel::shared::host_request_cancellation();
            let reply = match token {
                Some(token) => {
                    tokio::select! {
                        reply = &mut answer => reply,
                        () = token.cancelled() => {
                            cancel.cancel();
                            answer.await
                        }
                    }
                }
                None => answer.await,
            };
            reply.map_err(|error| anyhow!("bash host request failed: {error}"))
        });
    }
    let Some(handler) = handlers.get(request_type).cloned() else {
        let error =
            anyhow!("host request type \"{request_type}\" is not available in this session");
        return Box::pin(async move { Err(error) });
    };
    handler(HostRequestPayload { data: data.clone() })
}

impl Inner {
    /// Wait (bounded) for the in-flight host request tasks to settle.
    pub(crate) async fn wait_for_host_requests_to_settle(
        &self,
        tasks: Vec<tokio::task::JoinHandle<()>>,
        timeout_ms: u64,
    ) {
        let all = async {
            for task in tasks {
                let _ = task.await;
            }
        };
        if tokio::time::timeout(Duration::from_millis(timeout_ms), all)
            .await
            .is_err()
        {
            self.append_diagnostic(&format!(
                "timed out waiting {timeout_ms}ms for host request task(s) during shutdown"
            ));
        }
    }

    pub(crate) async fn wait_for_kernel_exit(&self) {
        let exit_rx = match lock(&self.child).as_ref() {
            Some(child) => child.exit_rx.clone(),
            None => return,
        };
        let mut exit_rx = exit_rx;
        if exit_rx.borrow().is_some() {
            return;
        }
        loop {
            if exit_rx.borrow().is_some() {
                return;
            }
            if exit_rx.changed().await.is_err() {
                return;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel ships `bash.consumed` ahead of the reading cell's `done`
    /// so the stale completion notice is withdrawn before the turn boundary
    /// probes the steering lane. The handler's synchronous body must
    /// therefore have run by the time the reader takes the next frame, not
    /// whenever a spawned task gets scheduled.
    #[tokio::test]
    async fn a_host_handler_runs_its_synchronous_body_before_the_next_frame() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use crate::kernel::manager::ReplKernelManager;
        use crate::kernel::protocol::Event;
        use crate::kernel::shared::{HostRequestHandlers, KernelManagerOptions, host_handler};

        let withdrawn = Arc::new(AtomicBool::new(false));
        let mut host_handlers = HostRequestHandlers::new();
        let flag = Arc::clone(&withdrawn);
        host_handlers.register(
            "bash.consumed",
            host_handler(move |_payload| {
                let flag = Arc::clone(&flag);
                async move {
                    flag.store(true, Ordering::SeqCst);
                    Ok(json!({}))
                }
            }),
        );
        let manager = ReplKernelManager::new(KernelManagerOptions {
            plan_guard: None,
            host_handlers,
            ..KernelManagerOptions::default()
        });

        manager.inner.handle_event(Event::HostRequest {
            id: "consumed-1".to_string(),
            data: json!({ "type": "bash.consumed", "pid": 42, "command": "ls" }),
        });

        assert!(withdrawn.load(Ordering::SeqCst));
    }
}
