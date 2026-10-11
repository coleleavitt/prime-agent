//! The `ravo.*` kernel host requests (TS `handleRavoHostRequest`): the
//! bundled `ravo` skill's `run`, `status` and `cancel`, and the `/ravo`
//! session command, over one [`RavoRunService`] per session, and the
//! session-model child the runs prompt. Every status update is published
//! as the session's `ravo` feature status (TS `ravo_run_update`).
//!
//! Like TS, RAVO runs are offered only where refine is
//! (`_autoRefineAllowedForSession`): a top-level session (RLM depth 0) with
//! a local harness store (a session artifact directory). Other sessions get
//! no handlers (the kernel's `ravo.*` calls fail as unregistered) and `/ravo`
//! refuses.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pa_core::features::{
    FeatureCommandOutcome,
    FeatureFuture,
    FeatureStatus,
    FeatureTelemetry,
    SessionFeatureContext,
};
use pa_core::kernel::shared::{HostRequestHandlers, host_handler};
use pa_core::models::ModelRegistry;
use pa_telemetry::Properties;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::command::{completion_text, parse_ravo_command, ravo_status_line, started_text};
use crate::referee::ReplayRunner;
use crate::run::{
    ModelFailure,
    ModelReply,
    RavoModel,
    RavoRunRequest,
    RavoRunService,
    RunServiceDeps,
    RunStores,
    parse_ravo_run_payload,
};

/// The adoption event: one finished `ravo.run` or `/ravo`.
pub const RAVO_RUN_EVENT: &str = "ravo_run";

/// The feature name a run's live status is published under.
pub const RAVO_STATUS_FEATURE: &str = "ravo";

/// The refusal where RAVO runs are not offered.
const UNAVAILABLE: &str = "RAVO is not available in this session";

/// Whether `context`'s session is offered RAVO runs (TS
/// `_autoRefineAllowedForSession`): RLM depth 0 with a local harness store.
#[must_use]
pub fn ravo_run_allowed(context: &SessionFeatureContext) -> bool {
    context.rlm_depth == 0 && context.session_artifact_dir.is_some()
}

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

/// A started run: its id, and its final status once it settles (`None`
/// when the run's task ended without one).
struct Launch {
    run_id: String,
    completion: tokio::sync::oneshot::Receiver<Option<Value>>,
}

impl RunHost {
    /// The session's service, where RAVO runs are offered
    /// ([`ravo_run_allowed`]), created on first use.
    fn service(&self, context: &SessionFeatureContext) -> Option<RavoRunService> {
        if !ravo_run_allowed(context) {
            return None;
        }
        let artifact_dir = context.session_artifact_dir.as_ref()?;
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
                    // Every update reaches the session's event surface (the
                    // daemon's `feature_status` event and the agents view).
                    on_update: Arc::new(move |status: &Value| {
                        pa_core::features::publish_feature_status(
                            &session_id,
                            FeatureStatus {
                                feature: RAVO_STATUS_FEATURE.to_string(),
                                line: Some(ravo_status_line(status)),
                                status: status.clone(),
                            },
                        );
                    }),
                })
            });
        Some(service.clone())
    }

    /// Start a run of `request` and watch it to its end, where the
    /// adoption event fires.
    fn launch(
        &self,
        context: &SessionFeatureContext,
        request: RavoRunRequest,
    ) -> Result<Launch, String> {
        let service = self
            .service(context)
            .ok_or_else(|| UNAVAILABLE.to_string())?;
        let scope = if request.global { "global" } else { "local" };
        let (run_id, handle) = service.start(request).map_err(|refused| refused.0)?;
        let (sender, completion) = tokio::sync::oneshot::channel();
        let telemetry = context.telemetry.clone();
        tokio::spawn(async move {
            let status = handle.await.ok();
            if let Some(telemetry) = telemetry {
                track(&telemetry, scope, status.as_ref().unwrap_or(&Value::Null));
            }
            let _ = sender.send(status);
        });
        Ok(Launch { run_id, completion })
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
                Ok(match self.launch(context, request) {
                    Err(reason) => json!({ "started": false, "reason": reason }),
                    Ok(launch) => json!({
                        "started": true,
                        "runId": launch.run_id,
                        "note": "The RAVO run continues in the background; check `ravo.status` or the Agents View for progress. Continue working normally.",
                    }),
                })
            }
            other => anyhow::bail!("unknown ravo request type \"{other}\""),
        }
    }

    /// `/ravo [--global] [--rounds N] [--repairs N] <task>` (TS
    /// `_executeQueuedSessionCommand`'s `ravo` case): the started row now,
    /// the terminal row when the run settles.
    fn command(
        &self,
        context: &SessionFeatureContext,
        args: &str,
    ) -> Result<FeatureCommandOutcome, String> {
        let command = parse_ravo_command(args)?;
        if command.arc_agi.is_some() {
            return Err(
                "RAVO's ARC-AGI evaluator (--arc-repo/--arc-game) is not part of this build"
                    .to_string(),
            );
        }
        let task = command.task.clone();
        let launch = self.launch(
            context,
            RavoRunRequest {
                task: command.task,
                instructions: None,
                global: command.global,
                max_rounds: command.max_rounds,
                max_repairs: command.max_repairs,
                deadline_ms: None,
                token_budget: None,
            },
        )?;
        let run_id = launch.run_id;
        let completion = launch.completion;
        let text = started_text(&run_id, &task);
        let completion: FeatureFuture<Result<String, String>> = Box::pin(async move {
            let status = completion.await.ok().flatten();
            completion_text(&run_id, status.as_ref())
        });
        Ok(FeatureCommandOutcome {
            text,
            completion: Some(completion),
        })
    }

    /// Cancel every session's running run (the process is exiting; TS
    /// cancelled it on dispose).
    pub fn cancel_all(&self) {
        for service in self
            .services
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            let _ = service.cancel();
        }
    }
}

/// Run `/ravo` for a session; a refusal or usage error is the `Err`.
pub(crate) fn execute_command(
    host: &Arc<RunHost>,
    context: &Arc<SessionFeatureContext>,
    args: &str,
) -> FeatureFuture<Result<FeatureCommandOutcome, String>> {
    let host = Arc::clone(host);
    let context = Arc::clone(context);
    let args = args.to_string();
    // Run on the session's runtime: the run is a task of it.
    Box::pin(async move { host.command(&context, &args) })
}

/// The adoption event for one settled run.
fn track(telemetry: &FeatureTelemetry, scope: &'static str, status: &Value) {
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
}

/// Register `ravo.run`, `ravo.status` and `ravo.cancel` for one session
/// offered RAVO runs ([`ravo_run_allowed`]).
pub(crate) fn register(
    host: &Arc<RunHost>,
    context: &SessionFeatureContext,
    handlers: &mut HostRequestHandlers,
) {
    if !ravo_run_allowed(context) {
        return;
    }
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
