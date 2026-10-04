//! The `ravo.*` kernel host requests (TS `handleRavoHostRequest`): the
//! bundled `ravo` skill's `run`, `status` and `cancel`, over one
//! [`RavoRunService`] per session, and the session-model child the runs
//! prompt.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pa_core::features::{FeatureTelemetry, SessionFeatureContext};
use pa_core::kernel::shared::{host_handler, HostRequestHandlers};
use pa_core::models::ModelRegistry;
use pa_telemetry::Properties;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::referee::ReplayRunner;
use crate::run::{
    parse_ravo_run_payload, ModelFailure, ModelReply, RavoModel, RavoRunService, RunServiceDeps,
    RunStores,
};

/// The adoption event: one finished `ravo.run`.
pub const RAVO_RUN_EVENT: &str = "ravo_run";

/// Where run status updates are logged (no daemon event carries them yet).
pub const RAVO_RUN_LOG_TARGET: &str = "pa_ravo::run";

/// The session model, resolved and authorized through the model registry
/// at each call (a credential refreshed meanwhile is picked up), one
/// tool-less provider call per prompt, no system prompt.
pub struct SessionModel {
    agent_dir: std::path::PathBuf,
    cwd: std::path::PathBuf,
    model: pa_agent::types::Model,
}

impl SessionModel {
    #[must_use]
    pub fn new(context: &SessionFeatureContext) -> Self {
        Self {
            agent_dir: context.agent_dir.clone(),
            cwd: context.cwd.clone(),
            model: context.model.clone(),
        }
    }
}

type Resolved = (
    pa_types::ai::Model,
    Option<String>,
    Option<BTreeMap<String, String>>,
);

fn resolve(
    agent_dir: &std::path::Path,
    cwd: std::path::PathBuf,
    session: &pa_agent::types::Model,
) -> Result<Resolved, String> {
    let mut registry = ModelRegistry::for_session(agent_dir, cwd);
    registry.load_private_authorization_from_cache();
    let model = registry
        .get_all()
        .iter()
        .find(|model| model.provider == session.provider && model.id == session.id)
        .cloned()
        .ok_or_else(|| {
            format!(
                "Model \"{}/{}\" is not registered",
                session.provider, session.id
            )
        })?;
    let auth = registry.get_api_key_and_headers(&model, None);
    if !auth.ok {
        return Err(auth
            .error
            .unwrap_or_else(|| format!("No credential found for \"{}\"", model.provider)));
    }
    let model = pa_core::session_engine::provider_adapter::json_round_trip(&model)
        .ok_or_else(|| "the resolved model failed the wire-shape conversion".to_string())?;
    Ok((model, auth.api_key, auth.headers))
}

impl RavoModel for SessionModel {
    fn complete(
        &self,
        prompt: String,
        _token_budget: u64,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ModelReply, ModelFailure>> + Send>> {
        let agent_dir = self.agent_dir.clone();
        let cwd = self.cwd.clone();
        let session = self.model.clone();
        Box::pin(async move {
            let failure = |status: &str, error: String| ModelFailure {
                status: status.to_string(),
                tokens: 0,
                error: Some(error),
            };
            let resolved = tokio::task::spawn_blocking(move || resolve(&agent_dir, cwd, &session))
                .await
                .map_err(|error| failure("error", error.to_string()))?
                .map_err(|error| failure("error", error))?;
            let (model, api_key, headers) = resolved;
            let context = pa_types::ai::Context {
                system_prompt: None,
                messages: vec![pa_types::ai::Message::User(pa_types::ai::UserMessage {
                    content: pa_types::ai::UserContent::Text(prompt),
                    timestamp: 0,
                    rest: serde_json::Map::default(),
                })],
                tools: None,
            };
            let options =
                pa_ai::types::SimpleStreamOptions::from_base(pa_ai::types::StreamOptions {
                    api_key,
                    headers: headers.map(|headers| headers.into_iter().collect()),
                    signal: Some(cancel.clone()),
                    ..Default::default()
                });
            let reply = tokio::select! {
                () = cancel.cancelled() => {
                    return Err(failure("aborted", "the RAVO run was cancelled".to_string()));
                }
                reply = pa_ai::complete_simple(&model, &context, Some(options)) => reply,
            };
            let reply = reply.map_err(|error| failure("error", error.to_string()))?;
            let tokens = reply.usage.total_tokens;
            if reply.stop_reason == pa_types::ai::StopReason::Error {
                return Err(ModelFailure {
                    status: "error".to_string(),
                    tokens,
                    error: reply.error_message,
                });
            }
            let text = reply
                .content
                .iter()
                .filter_map(|block| match block {
                    pa_types::ai::AssistantContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            Ok(ModelReply { text, tokens })
        })
    }
}

/// Builds a session's run model (tests script it).
pub type ModelFactory = Arc<dyn Fn(&SessionFeatureContext) -> Arc<dyn RavoModel> + Send + Sync>;

/// The run services of every session.
pub(crate) struct RunHost {
    pub runner: Arc<dyn ReplayRunner>,
    pub replay_sys_path: Vec<String>,
    pub model: Mutex<ModelFactory>,
    pub services: Mutex<HashMap<String, RavoRunService>>,
}

fn not_available() -> Value {
    json!({ "started": false, "reason": "RAVO is not available in this session" })
}

impl RunHost {
    /// The session's service, where `ravo.run` is available: a top-level
    /// session with a local harness store (TS: where refine is).
    fn service(&self, context: &SessionFeatureContext) -> Option<RavoRunService> {
        let artifact_dir = context.session_artifact_dir.as_ref()?;
        if context.rlm_depth != 0 {
            return None;
        }
        let mut services = self
            .services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let service = services
            .entry(context.session_id.clone())
            .or_insert_with(|| {
                let session_id = context.session_id.clone();
                let factory = Arc::clone(
                    &self
                        .model
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
                RavoRunService::new(RunServiceDeps {
                    model: factory(context),
                    runner: Arc::clone(&self.runner),
                    replay_sys_path: self.replay_sys_path.clone(),
                    stores: RunStores {
                        local_dir: pa_ledger::local_harness_state_dir(artifact_dir),
                        global_dir: pa_ledger::global_harness_state_dir(&context.agent_dir),
                        agent_dir: context.agent_dir.clone(),
                    },
                    on_update: Arc::new(move |status: &Value| {
                        tracing::debug!(
                            target: RAVO_RUN_LOG_TARGET,
                            session_id = session_id.as_str(),
                            status = %status,
                            "ravo_run_update"
                        );
                    }),
                })
            });
        Some(service.clone())
    }

    fn handle(
        &self,
        context: &SessionFeatureContext,
        kind: &str,
        payload: &Value,
    ) -> anyhow::Result<Value> {
        match kind {
            "ravo.status" => Ok(self
                .service(context)
                .and_then(|service| service.status())
                .unwrap_or_else(|| json!({ "phase": "idle" }))),
            "ravo.cancel" => Ok(json!({
                "cancelled": self.service(context).is_some_and(|service| service.cancel())
            })),
            "ravo.run" => {
                let request = parse_ravo_run_payload(payload).map_err(anyhow::Error::msg)?;
                let Some(service) = self.service(context) else {
                    return Ok(not_available());
                };
                let scope = if request.global { "global" } else { "local" };
                match service.start(request) {
                    Err(refused) => Ok(json!({ "started": false, "reason": refused.0 })),
                    Ok((run_id, completion)) => {
                        track_completion(context.telemetry.clone(), scope, completion);
                        Ok(json!({
                            "started": true,
                            "runId": run_id,
                            "note": "The RAVO run continues in the background; check `ravo.status` or the Agents View for progress. Continue working normally.",
                        }))
                    }
                }
            }
            other => anyhow::bail!("unknown ravo request type \"{other}\""),
        }
    }
}

/// The adoption event once the run settles.
fn track_completion(
    telemetry: Option<FeatureTelemetry>,
    scope: &'static str,
    completion: tokio::task::JoinHandle<Value>,
) {
    tokio::spawn(async move {
        let status = completion.await.unwrap_or(Value::Null);
        let Some(telemetry) = telemetry else {
            return;
        };
        let outcome = if status.get("error").is_some() {
            "error".to_string()
        } else {
            status
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string()
        };
        let mut properties = Properties::new();
        properties.set("outcome", json!(outcome));
        properties.set("scope", json!(scope));
        properties.set(
            "rounds",
            json!(status.get("round").and_then(Value::as_u64).unwrap_or(0)),
        );
        properties.set(
            "repairs",
            json!(status.get("repairs").and_then(Value::as_u64).unwrap_or(0)),
        );
        telemetry.track(RAVO_RUN_EVENT, &properties);
    });
}

/// Register `ravo.run`, `ravo.status` and `ravo.cancel` for one session.
pub(crate) fn register(
    host: &Arc<RunHost>,
    context: &SessionFeatureContext,
    handlers: &mut HostRequestHandlers,
) {
    let context = Arc::new(context.clone());
    for kind in ["ravo.run", "ravo.status", "ravo.cancel"] {
        let host = Arc::clone(host);
        let context = Arc::clone(&context);
        handlers.register(
            kind,
            host_handler(move |payload| {
                let host = Arc::clone(&host);
                let context = Arc::clone(&context);
                async move { host.handle(&context, kind, &payload.data) }
            }),
        );
    }
}
