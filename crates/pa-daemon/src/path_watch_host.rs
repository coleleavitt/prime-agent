//! The kernel host handlers of the session-owned path watches (upstream
//! #2351): `rlm.watch.path`, `rlm.watch.path_list`, `rlm.watch.path_get`,
//! `rlm.watch.path_cancel`.
//!
//! The engine owns the registry (the session, not the kernel, holds the
//! subscriptions, so a kernel restart never drops them); the receiving
//! WORKER owns the notice routing, so it installs the event sink
//! ([`AgentSessionEngine::set_path_watch_sink`]) at construction, and the
//! engine registers the handlers only when the sink exists.

use pa_types::sync::MutexExt;
use std::sync::Arc;

use serde_json::{json, Value};

use pa_core::kernel::shared::{host_handler, HostRequestHandlers};

use crate::agent_engine::AgentSessionEngine;
use crate::path_watch::PathWatchSink;

/// One request's non-empty `watch_id`.
fn watch_id(request: &str, data: &Value) -> anyhow::Result<String> {
    match data.get("watch_id").and_then(Value::as_str).map(str::trim) {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => anyhow::bail!("{request} watch_id must be a non-empty string"),
    }
}

impl AgentSessionEngine {
    /// Install the path-watch event routing (the worker calls this at
    /// construction, before the first session build registers the handlers).
    pub fn set_path_watch_sink(&self, sink: PathWatchSink) {
        *self.path_watch_sink.lock_or_recover() = Some(sink);
    }

    /// Release every path watch (the "watchers die with the session" rule:
    /// a session close or replacement).
    pub fn dispose_path_watches(&self) {
        self.path_watches.dispose();
    }

    pub(crate) fn register_path_watch_host_handlers(&self, handlers: &mut HostRequestHandlers) {
        let Some(sink) = self.path_watch_sink.lock_or_recover().clone() else {
            return;
        };
        // The handlers hold the engine weakly (the registered self-arc):
        // the session owns the registry, a watch never pins the engine.
        let Some(weak) = self.self_weak.lock_or_recover().clone() else {
            return;
        };
        let register_weak = weak.clone();
        handlers.register(
            "rlm.watch.path",
            host_handler(move |payload| {
                let engine = register_weak.clone();
                let sink = Arc::clone(&sink);
                Box::pin(async move {
                    let Some(engine) = engine.upgrade() else {
                        anyhow::bail!("watch registration failed: session is closing");
                    };
                    let path = match payload.data.get("path").and_then(Value::as_str) {
                        Some(path) if !path.trim().is_empty() => path.trim().to_string(),
                        _ => anyhow::bail!("rlm.watch.path path must be a non-empty string"),
                    };
                    let recursive = match payload.data.get("recursive") {
                        None | Some(Value::Null) => false,
                        Some(Value::Bool(recursive)) => *recursive,
                        Some(_) => anyhow::bail!("rlm.watch.path recursive must be a boolean"),
                    };
                    let resolved = crate::path_watch::resolve_watch_path(&path, &engine.cwd());
                    let watch = engine.path_watches.register(&resolved, recursive, sink)?;
                    if let Some(telemetry) = engine.session_telemetry.lock_or_recover().clone() {
                        telemetry.note_adoption(
                            pa_core::session_engine::telemetry::SessionAdoption::PathWatchRegistered,
                        );
                    }
                    Ok(json!({ "watch": watch.host_response() }))
                })
            }),
        );
        let list_weak = weak.clone();
        handlers.register(
            "rlm.watch.path_list",
            host_handler(move |_payload| {
                let engine = list_weak.clone();
                Box::pin(async move {
                    let Some(engine) = engine.upgrade() else {
                        anyhow::bail!("watch listing failed: session is closing");
                    };
                    let watches = engine
                        .path_watches
                        .list()
                        .iter()
                        .map(crate::path_watch::PathWatchInfo::host_response)
                        .collect::<Vec<_>>();
                    Ok(json!({ "watches": watches }))
                })
            }),
        );
        let get_weak = weak.clone();
        handlers.register(
            "rlm.watch.path_get",
            host_handler(move |payload| {
                let engine = get_weak.clone();
                Box::pin(async move {
                    let Some(engine) = engine.upgrade() else {
                        anyhow::bail!("watch lookup failed: session is closing");
                    };
                    let id = watch_id("rlm.watch.path_get", &payload.data)?;
                    let Some(watch) = engine.path_watches.get(&id) else {
                        anyhow::bail!("Unknown path watch: {id}");
                    };
                    Ok(json!({ "watch": watch.host_response() }))
                })
            }),
        );
        handlers.register(
            "rlm.watch.path_cancel",
            host_handler(move |payload| {
                let engine = weak.clone();
                Box::pin(async move {
                    let Some(engine) = engine.upgrade() else {
                        anyhow::bail!("watch cancel failed: session is closing");
                    };
                    let id = watch_id("rlm.watch.path_cancel", &payload.data)?;
                    let watch = engine.path_watches.cancel(&id)?;
                    Ok(json!({ "watch": watch.host_response() }))
                })
            }),
        );
    }
}
