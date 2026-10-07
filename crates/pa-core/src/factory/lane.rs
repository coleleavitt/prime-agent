//! The `factory_activity` lane: the daemon/TUI's `/factory` view over one
//! session's executor (graph/status/watch/run/stop/resume).
//!
//! The lane answers host-side, with no kernel round-trip: the runs live in
//! the host, so the view keeps working while the kernel restarts. Replies
//! carry the wire's camelCase keys ([`wire_payload`]) under the lane's
//! frame cap ([`cap_factory_frame`]); the in-kernel `rlm.factory` API stays
//! `snake_case`.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::executor::snapshot::{cap_factory_frame, wire_payload};
use super::executor::{FactoryExecutor, FactoryRefusal, ResolvedSubagent, RunRequest};
use super::pyvalue::{py_repr, py_str_repr, PyValue};

/// The single refusal every gated factory surface answers while the
/// `factory.enabled` setting is off.
pub const FACTORY_DISABLED_MESSAGE: &str = "the factory is disabled; run /factory on to enable it";
/// The lane's action vocabulary.
pub const ACTIVITY_ACTIONS: [&str; 6] = ["graph", "status", "watch", "run", "stop", "resume"];
/// Upper bound on one watch's `timeoutMs` on the lane.
pub const ACTIVITY_TIMEOUT_MS_CAP: u64 = 60_000;

/// One stored factory entry the lane can run or graph: its id (scope
/// prefix stripped), its spec, and its resolved subagent references.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSpec {
    pub id: String,
    pub spec: PyValue,
    pub subagents: HashMap<String, Option<ResolvedSubagent>>,
}

/// What the lane reads outside the executor: the opt-in gate and the
/// session's stored factory entries.
///
/// Implementations read fresh state per call (the setting toggles with
/// `/factory on|off`; the harness store changes under the kernel).
pub trait FactoryLaneContext: Send + Sync {
    /// Whether the `factory.enabled` setting is on.
    fn factory_enabled(&self) -> bool;
    /// The stored factory entry `spec_id` names, if any.
    fn stored_spec(&self, spec_id: &str) -> Option<StoredSpec>;
}

/// Read the `factory.enabled` opt-in from an agent dir's `settings.json`
/// (default off; a missing file or key, a wrong-typed value, or a corrupt
/// document all read as disabled — the kernel gate's exact rule).
#[must_use]
pub fn factory_enabled_in(agent_dir: &std::path::Path) -> bool {
    std::fs::read(agent_dir.join("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|document| document.get("factory")?.get("enabled")?.as_bool())
        .unwrap_or(false)
}

fn refusal(message: impl Into<String>) -> FactoryRefusal {
    FactoryRefusal(message.into())
}

/// One graph by reference: a live run's snapshot, else a stored spec's
/// static structure; `None` for every reportable run.
fn graph(
    executor: &FactoryExecutor,
    context: &dyn FactoryLaneContext,
    reference: Option<&str>,
) -> Result<Value, FactoryRefusal> {
    if let Some(snapshot) = executor.graph(reference, /*compact*/ true) {
        return Ok(snapshot);
    }
    let reference = reference.unwrap_or_default();
    let stored = context.stored_spec(reference).ok_or_else(|| {
        refusal(format!(
            "unknown factory run or spec {}",
            py_str_repr(reference)
        ))
    })?;
    executor.spec_graph(reference, &stored.id, &stored.spec)
}

/// Handle one lane request (`{"action", "runId"?, "specId"?,
/// "timeoutMs"?}`) and answer the reply's result payload in wire keys.
///
/// # Errors
///
/// The disabled-factory refusal, a malformed request, or the executor's
/// refusal (unknown runs and specs, invalid specs, a resume of a run that
/// is not paused).
pub async fn activity(
    executor: &FactoryExecutor,
    context: &dyn FactoryLaneContext,
    request: &Value,
) -> Result<Value, FactoryRefusal> {
    if !context.factory_enabled() {
        return Err(refusal(FACTORY_DISABLED_MESSAGE));
    }
    let field = |key: &str| request.get(key).filter(|value| !value.is_null());
    let action = field("action").cloned().unwrap_or(Value::Null);
    let Some(action) = action
        .as_str()
        .filter(|action| ACTIVITY_ACTIONS.contains(action))
    else {
        return Err(refusal(format!(
            "unknown factory activity action {}",
            py_repr(&PyValue::from_json(&action))
        )));
    };
    let mut ids = [None, None];
    for (slot, key) in ids.iter_mut().zip(["runId", "specId"]) {
        match field(key) {
            None => {}
            Some(Value::String(id)) => *slot = Some(id.as_str()),
            Some(_) => {
                return Err(refusal(format!(
                    "factory activity {key} must be a string when provided"
                )))
            }
        }
    }
    let [run_id, spec_id] = ids;
    let timeout_ms = match field("timeoutMs") {
        None => Some(0),
        Some(value) => value
            .as_u64()
            .filter(|timeout| *timeout <= ACTIVITY_TIMEOUT_MS_CAP),
    }
    .ok_or_else(|| {
        refusal(format!(
            "factory activity timeoutMs must be an integer between 0 and {ACTIVITY_TIMEOUT_MS_CAP}"
        ))
    })?;
    let run_id = run_id.filter(|id| !id.is_empty());
    let spec_id = spec_id.filter(|id| !id.is_empty());
    let require_run = |action: &str| {
        run_id.ok_or_else(|| refusal(format!("factory activity {action} requires runId")))
    };
    let result = match action {
        "graph" => graph(executor, context, run_id.or(spec_id))?,
        "status" => executor.status(require_run(action)?)?,
        "watch" => {
            executor
                .watch(
                    require_run(action)?,
                    Some(timeout_ms as f64 / 1000.0),
                    /*compact*/ true,
                )
                .await?
        }
        "run" => {
            let spec_id = spec_id.ok_or_else(|| refusal("factory activity run requires specId"))?;
            let stored = context
                .stored_spec(spec_id)
                .ok_or_else(|| refusal(format!("unknown factory spec {}", py_str_repr(spec_id))))?;
            executor
                .run(RunRequest {
                    spec_id: stored.id,
                    name: None,
                    spec: stored.spec,
                    subagents: stored.subagents,
                    library: None,
                })
                .await?
        }
        "stop" => executor.stop(require_run(action)?).await?,
        _ => executor.resume(require_run(action)?).await?,
    };
    Ok(wire_payload(&result))
}

/// Bound one lane reply under the frame cap: an ok reply trims its event
/// tails (or sheds terminal run rows) to fit, and a reply or an error that
/// still cannot fit fails loudly with the wire-cap reason.
///
/// # Errors
///
/// The refusal's sentence, or the wire-cap failure.
pub fn capped_reply(reply: Result<Value, FactoryRefusal>) -> Result<Value, String> {
    let mut frame = match reply {
        Ok(result) => json!({ "status": "ok", "result": result }),
        Err(FactoryRefusal(reason)) => json!({ "status": "error", "reason": reason }),
    };
    cap_factory_frame(&mut frame);
    if frame["status"] == "ok" {
        return Ok(frame["result"].take());
    }
    Err(frame["reason"].as_str().unwrap_or_default().to_string())
}
