//! The `workflow.run_agent` kernel host-request handler (TS
//! `agent-session.ts` `"workflow.run_agent"`): decode the closed request,
//! preflight the model and credential, run the one isolated provider turn,
//! and settle with the closed reply.
//!
//! Isolation (TS final review B2): the turn uses the provider transport
//! directly — never the session's own stream (its retry driver, request
//! timing, or semantic-edge recorder) — so it leaves no trace in the
//! session's messages, ledgers, or files. Provider I/O is its only effect.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pa_agent::stream::StreamFn;
use pa_core::features::FeatureTelemetry;
use pa_core::kernel::shared::host_request_cancellation;
use pa_core::models::ModelRegistry;
use pa_core::session_engine::provider_adapter::stream_once;
use pa_telemetry::Properties;
use serde_json::{json, Value};

use super::preflight::{preflight, Cleared};
use super::runner::{run_turn, Turn};
use super::wire::{
    budget, decode_request, reply, FailureReason, Finality, RunAgentRequest, Settlement, Terminal,
    Usage, ZeroCost,
};

/// The host-request type the runtime sends (`rlm/workflow.py`).
pub const RUN_AGENT_REQUEST_TYPE: &str = "workflow.run_agent";
/// The adoption event (`pa-telemetry` catalog v4).
const TELEMETRY_EVENT: &str = "workflow_run_agent";

/// The session facts one handler resolves against.
pub(crate) struct HostConfig {
    pub agent_dir: PathBuf,
    pub cwd: PathBuf,
    pub session_model: pa_agent::types::Model,
    pub telemetry: Option<FeatureTelemetry>,
}

/// Handle one `workflow.run_agent` request.
///
/// # Errors
///
/// Returns an error only when the request violates the closed V1 contract
/// (the runtime then reports the capability unavailable); every admitted
/// request settles with a reply, failures included.
#[tracing::instrument(name = "workflow.run_agent", skip_all)]
pub(crate) async fn handle_run_agent(
    config: Arc<HostConfig>,
    payload: Value,
) -> anyhow::Result<Value> {
    let started = Instant::now();
    // Read in the handler's own future: the token is task-local.
    let cancel = host_request_cancellation().unwrap_or_default();
    let request = decode_request(payload.get("request").unwrap_or(&Value::Null))?;
    let cleared = {
        let config = Arc::clone(&config);
        let reference = request.model.clone();
        tokio::task::spawn_blocking(move || {
            let mut registry = ModelRegistry::for_session(&config.agent_dir, config.cwd.clone());
            registry.load_private_authorization_from_cache();
            preflight(&mut registry, reference.as_deref(), &config.session_model)
        })
        .await?
    };
    let settlement = match cleared {
        Err(message) => Settlement {
            resolved_model: None,
            turns_started: 0,
            duration_ms: elapsed_ms(started),
            usage: Usage::zero(Finality::Final, ZeroCost::Unobserved),
            terminal: Terminal::failed(FailureReason::ModelResolutionFailed, &message),
        },
        Ok(cleared) => {
            let resolved_model = Some(cleared.selector());
            let report = run_turn(Turn {
                prompt: request.prompt.clone(),
                model: pa_core::session_engine::provider_adapter::json_round_trip(&cleared.model)
                    .ok_or_else(|| {
                    anyhow::anyhow!("the resolved model failed the wire-shape conversion")
                })?,
                stream_fn: provider_transport(&cleared),
                api_key: cleared.api_key,
                headers: cleared.headers,
                cancel,
                drain_timeout: Duration::from_millis(request.drain_timeout_ms),
                max_result_utf8_bytes: request.max_result_utf8_bytes,
            })
            .await;
            Settlement {
                resolved_model,
                turns_started: report.turns_started,
                duration_ms: elapsed_ms(started),
                usage: report.usage,
                terminal: report.terminal,
            }
        }
    };
    record(config.telemetry.as_ref(), &request, &settlement);
    Ok(reply(&request, &settlement))
}

/// The provider transport bound to the cleared model: one physical
/// request per call, no retry, the request's own credential and headers.
fn provider_transport(cleared: &Cleared) -> StreamFn {
    let model = cleared.model.clone();
    Arc::new(move |_requested, context, options| {
        let model = model.clone();
        Box::pin(async move {
            let api_key = options.api_key.clone();
            stream_once(&model, api_key, None, None, context, options)
        })
    })
}

/// The settle event: a `tracing` record for the local trace, and the
/// adoption event (classification and totals only, never content or ids).
fn record(
    telemetry: Option<&FeatureTelemetry>,
    request: &RunAgentRequest,
    settlement: &Settlement,
) {
    let outcome = settlement.terminal.outcome();
    let stop_reason = settlement.terminal.stop_reason();
    let (budget_exhausted, _) = budget(request, settlement.usage.total_tokens);
    tracing::info!(
        target: "pa_workflow",
        outcome,
        stop_reason,
        turns_started = settlement.turns_started,
        duration_ms = settlement.duration_ms,
        total_tokens = settlement.usage.total_tokens,
        "workflow.run_agent settled"
    );
    let Some(telemetry) = telemetry else {
        return;
    };
    let mut properties = Properties::new();
    properties.set("outcome", json!(outcome));
    properties.set("stop_reason", json!(stop_reason));
    properties.set("turns_started", json!(settlement.turns_started));
    properties.set("duration_ms", json!(settlement.duration_ms));
    properties.set("total_tokens", json!(settlement.usage.total_tokens));
    properties.set("budget_exhausted", json!(budget_exhausted));
    telemetry.track(TELEMETRY_EVENT, &properties);
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
