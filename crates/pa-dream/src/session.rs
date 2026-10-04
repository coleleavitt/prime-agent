//! The in-session Dream-RSI feature (TS `agent-session.ts`'s dream wiring):
//! the kernel skill's `dream.run` / `dream.status` / `dream.cancel` /
//! `dream.experiment` host requests, the `/dream` session command, the
//! bundled `dream` skill, and the adoption event.
//!
//! Like the TS product, Dream is offered only to sessions that may improve
//! themselves: a top-level session (RLM depth 0) with a local harness state
//! (a session artifact directory). Other sessions get no handlers (the
//! kernel's `dream.*` calls fail as unregistered) and `/dream` refuses.
//! Each such session owns one [`DreamRunService`] (one run slot), created on
//! first use.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use pa_core::features::{
    FeatureCommandOutcome, FeatureFuture, FeatureTelemetry, SessionFeature, SessionFeatureContext,
};
use pa_core::kernel::shared::{host_handler, HostRequestHandlers};
use pa_telemetry::Properties;
use pa_types::slash_commands::{BuiltinSlashCommand, SlashCommandExecution};
use serde_json::{json, Map, Value};

use crate::agent_runner::AgentRunAgent;
use crate::requests::{
    parse_dream_command, parse_experiment_payload, parse_run_payload, DreamCommand,
};
use crate::run_service::{
    DreamExperimentRequest, DreamRunKind, DreamRunRequest, DreamRunService, DreamRunServiceDeps,
    DreamRunStatus, DreamStopReason, StartedRun,
};
use crate::store::ENV_DREAM_DIR;
use crate::tasks::DreamTaskId;

/// The host-request types the bundled `dream` skill sends.
pub const DREAM_REQUEST_TYPES: [&str; 4] = [
    "dream.run",
    "dream.experiment",
    "dream.status",
    "dream.cancel",
];
/// The bundled kernel skill (under `skills/.features/`).
pub const DREAM_SKILL: &str = "dream";
/// The session command.
pub const DREAM_COMMAND: &str = "dream";
const DREAM_COMMAND_DESCRIPTION: &str = "Run the Dream-RSI loop over a scored task; --llm-proposer/--llm-dreamer spend tokens, default is local and token-free";
const DREAM_COMMAND_HINT: &str = "[experiment] [--task <id>] [--n N] [--seed N] [--seeds a,b,c] [--iterations N] [--rounds N] [--arms dream,fixed] [--workers N] [--k1 N] [--k2 N] [--dreams N] [--priming none|diverse] [--model provider/id] [--thinking <level>] [--max-output-tokens N] [--llm-proposer] [--llm-dreamer]";
const TELEMETRY_EVENT: &str = "dream_session_run";
const UNAVAILABLE: &str = "Dream-RSI is not available in this session";

/// The feature `pa-cli` installs behind its `dream` Cargo feature.
#[derive(Default)]
pub struct DreamFeature {
    services: Mutex<HashMap<String, DreamRunService>>,
}

impl DreamFeature {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// The agents-view line of a status (TS `formatDreamRunStatusLine`).
#[must_use]
pub fn dream_status_line(status: &DreamRunStatus) -> String {
    let experiment = status.kind == DreamRunKind::Experiment;
    let kind = if experiment {
        "dream experiment"
    } else {
        "dream"
    };
    let phase = serde_json::to_value(status.phase)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();
    if let Some(reason) = status.stop_reason {
        let results = status
            .result_paths
            .as_ref()
            .map_or(usize::from(status.result_path.is_some()), Vec::len);
        let suffix = if experiment && results > 0 {
            format!(
                " ({results} result file{})",
                if results == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        return format!("{kind} {}{suffix}", reason.as_str());
    }
    if status.error.is_some() {
        return format!("{kind} error");
    }
    let best = crate::json::to_fixed(status.best_node_score, 4);
    if experiment {
        let seed = match (status.seed_index, status.seed_count) {
            (Some(index), Some(count)) => format!(" seed {}/{count}", index + 1),
            _ => String::new(),
        };
        let arm = status.arm.map_or_else(String::new, |arm| {
            match (status.arm_index, status.arm_count) {
                (Some(index), Some(count)) => format!(" {} {}/{count}", arm.as_str(), index + 1),
                _ => format!(" {}", arm.as_str()),
            }
        });
        let round = status.rounds.map_or_else(String::new, |rounds| {
            format!(" r{}/{rounds}", status.round.unwrap_or(0))
        });
        let tokens = status
            .tokens
            .map_or_else(String::new, |tokens| format!(" tokens {tokens}"));
        return format!("{kind}{seed}{arm}{round} {phase} best {best}{tokens}");
    }
    let suffix = status.final_policy_score.map_or_else(String::new, |score| {
        format!(
            " final {} improved {}",
            crate::json::to_fixed(score, 4),
            status.improved.unwrap_or(false)
        )
    });
    format!("dream {phase} it{} best {best}{suffix}", status.iteration)
}

/// Whether `context`'s session may run Dream-RSI (TS `_autoRefineAllowedForSession`).
#[must_use]
pub fn dream_allowed(context: &SessionFeatureContext) -> bool {
    context.rlm_depth == 0 && context.session_artifact_dir.is_some()
}

/// The dream store for a session: `$PRIME_AGENT_DREAM_DIR`, else `<agent dir>/dream`.
#[must_use]
pub fn session_dream_dir(agent_dir: &Path) -> PathBuf {
    match std::env::var_os(ENV_DREAM_DIR).filter(|dir| !dir.is_empty()) {
        Some(_) => crate::store::dream_dir(),
        None => agent_dir.join("dream"),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// What a launch reports back to its caller.
struct Launch {
    run_id: String,
    completion: tokio::sync::oneshot::Receiver<Result<DreamRunStatus, String>>,
}

/// The facts the adoption event records about one launch.
struct LaunchFacts {
    kind: DreamRunKind,
    surface: &'static str,
    task: DreamTaskId,
    llm_proposer: bool,
    llm_dreamer: bool,
    seeds: usize,
}

struct SessionDream {
    feature: Arc<DreamFeatureShared>,
}

/// The parts of the feature a session's handlers share.
struct DreamFeatureShared {
    service: DreamRunService,
    telemetry: Option<FeatureTelemetry>,
}

impl DreamFeature {
    /// The session's service, created on first use; `None` outside a runtime.
    fn session(&self, context: &Arc<SessionFeatureContext>) -> Option<SessionDream> {
        let mut services = self.services.lock().unwrap_or_else(PoisonError::into_inner);
        let service = if let Some(service) = services.get(&context.session_id) {
            service.clone()
        } else {
            let runtime = tokio::runtime::Handle::try_current().ok()?;
            let runner = AgentRunAgent {
                agent_dir: context.agent_dir.clone(),
                cwd: context.cwd.clone(),
                session_model: context.model.clone(),
                runtime,
            };
            let session_id = context.session_id.clone();
            let service = DreamRunService::new(DreamRunServiceDeps {
                runner: Arc::new(runner),
                session_model: Some(format!("{}/{}", context.model.provider, context.model.id)),
                dir: session_dream_dir(&context.agent_dir),
                now: Arc::new(now_ms),
                rng: None,
                llm_experiments: true,
                // Every snapshot reaches the session's event surface (the
                // daemon's `feature_status` event and the agents view).
                on_update: Arc::new(move |status| {
                    pa_core::features::publish_feature_status(
                        &session_id,
                        pa_core::features::FeatureStatus {
                            feature: "dream".to_string(),
                            line: Some(dream_status_line(status)),
                            status: json!(status),
                        },
                    );
                }),
            });
            services.insert(context.session_id.clone(), service.clone());
            service
        };
        Some(SessionDream {
            feature: Arc::new(DreamFeatureShared {
                service,
                telemetry: context.telemetry.clone(),
            }),
        })
    }
}

impl DreamFeatureShared {
    /// Start a run or experiment and watch it to its end: the adoption event
    /// fires there, and the receiver settles with the terminal status.
    fn launch(
        &self,
        started: Result<StartedRun, String>,
        facts: LaunchFacts,
    ) -> Result<Launch, String> {
        let started = started?;
        let (sender, completion) = tokio::sync::oneshot::channel();
        let telemetry = self.telemetry.clone();
        let begun = Instant::now();
        let run_id = started.run_id.clone();
        std::thread::Builder::new()
            .name("dream-run-watch".to_string())
            .spawn(move || {
                let result = started
                    .completion
                    .join()
                    .unwrap_or_else(|_| Err("the Dream-RSI run thread panicked".to_string()));
                if let Some(telemetry) = telemetry {
                    track(&telemetry, &facts, &result, begun);
                }
                let _ = sender.send(result);
            })
            .map_err(|error| format!("could not watch the Dream-RSI run: {error}"))?;
        Ok(Launch { run_id, completion })
    }
}

fn track(
    telemetry: &FeatureTelemetry,
    facts: &LaunchFacts,
    result: &Result<DreamRunStatus, String>,
    begun: Instant,
) {
    let (outcome, improved) = match result {
        Ok(status) if status.stop_reason == Some(DreamStopReason::Cancelled) => {
            ("cancelled", false)
        }
        Ok(status) => ("completed", status.improved.unwrap_or(false)),
        Err(_) => ("failed", false),
    };
    let mut properties = Properties::new();
    properties.set(
        "kind",
        json!(match facts.kind {
            DreamRunKind::Run => "run",
            DreamRunKind::Experiment => "experiment",
        }),
    );
    properties.set("surface", json!(facts.surface));
    properties.set("task", json!(facts.task.as_str()));
    properties.set("outcome", json!(outcome));
    properties.set("llm_proposer", json!(facts.llm_proposer));
    properties.set("llm_dreamer", json!(facts.llm_dreamer));
    properties.set("seeds", json!(facts.seeds));
    properties.set("improved", json!(improved));
    properties.set(
        "duration_ms",
        json!(u64::try_from(begun.elapsed().as_millis()).unwrap_or(u64::MAX)),
    );
    telemetry.track(TELEMETRY_EVENT, &properties);
}

fn run_facts(request: &DreamRunRequest, surface: &'static str) -> LaunchFacts {
    LaunchFacts {
        kind: DreamRunKind::Run,
        surface,
        task: request.task,
        llm_proposer: request.llm_proposer,
        llm_dreamer: request.llm_dreamer,
        seeds: 1,
    }
}

fn experiment_facts(request: &DreamExperimentRequest, surface: &'static str) -> LaunchFacts {
    LaunchFacts {
        kind: DreamRunKind::Experiment,
        surface,
        task: request.task,
        llm_proposer: request.llm_proposer,
        llm_dreamer: request.llm_dreamer,
        seeds: request.seeds.as_ref().map_or(1, Vec::len),
    }
}

/// The launching turn's trace id, read on the caller's thread.
fn trigger() -> Option<String> {
    pa_types::trace_context::current().map(|context| context.trace_id_hex())
}

fn payload_object(payload: &Value) -> Map<String, Value> {
    payload.as_object().cloned().unwrap_or_default()
}

/// One `dream.*` host request (TS `handleDreamHostRequest`).
fn handle(session: &SessionDream, request_type: &str, payload: &Value) -> Result<Value, String> {
    let service = &session.feature.service;
    match request_type {
        "dream.status" => Ok(service
            .status()
            .map_or_else(|| json!({ "phase": "idle" }), |status| json!(status))),
        "dream.cancel" => Ok(json!({ "cancelled": service.cancel() })),
        "dream.run" => {
            let request = parse_run_payload(&payload_object(payload))?;
            let facts = run_facts(&request, "skill");
            Ok(
                match session
                    .feature
                    .launch(service.start(request, trigger()), facts)
                {
                    Ok(launch) => json!({
                        "started": true,
                        "runId": launch.run_id,
                        "note": "The Dream-RSI run continues in the background; check `dream.status` or the Agents View for progress. Continue working normally.",
                    }),
                    Err(reason) => json!({ "started": false, "reason": reason }),
                },
            )
        }
        "dream.experiment" => {
            let request = parse_experiment_payload(&payload_object(payload))?;
            let seeds = request.seeds.as_ref().map(Vec::len);
            let facts = experiment_facts(&request, "skill");
            Ok(
                match session
                    .feature
                    .launch(service.start_experiment(request, trigger()), facts)
                {
                    Ok(launch) => {
                        let seed_note = seeds.map_or_else(String::new, |count| {
                            format!(" ({count} seeds, run sequentially)")
                        });
                        let mut reply = json!({ "started": true, "runId": launch.run_id });
                        if let Some(count) = seeds {
                            reply["seeds"] = json!(count);
                        }
                        reply["note"] = json!(format!(
                        "The Dream-RSI experiment{seed_note} continues in the background; check `dream.status` (resultPath/resultPaths on completion) or the Agents View for progress. Continue working normally."
                    ));
                        reply
                    }
                    Err(reason) => json!({ "started": false, "reason": reason }),
                },
            )
        }
        other => Err(format!("unknown dream request type \"{other}\"")),
    }
}

/// The terminal row of a `/dream` run (TS `_reportDreamRunCompletion`).
fn completion_text(
    kind: &str,
    run_id: &str,
    result: Result<DreamRunStatus, String>,
) -> Result<String, String> {
    match result {
        Ok(status) => {
            let paths = status
                .result_paths
                .clone()
                .filter(|paths| !paths.is_empty())
                .or_else(|| status.result_path.clone().map(|path| vec![path]))
                .unwrap_or_default();
            let reason = status
                .stop_reason
                .map_or("stopped", DreamStopReason::as_str);
            let results = if paths.is_empty() {
                String::new()
            } else {
                format!(" (results {})", paths.join(", "))
            };
            Ok(format!("Dream-RSI {kind} {run_id} {reason}{results}"))
        }
        Err(error) => Err(format!("Dream-RSI {kind} {run_id} failed: {error}")),
    }
}

fn completion_future(kind: &'static str, launch: Launch) -> FeatureFuture<Result<String, String>> {
    let run_id = launch.run_id;
    Box::pin(async move {
        let result = launch
            .completion
            .await
            .unwrap_or_else(|_| Err("the Dream-RSI run ended without a status".to_string()));
        completion_text(kind, &run_id, result)
    })
}

/// `/dream [experiment] ...` (TS `_executeQueuedSessionCommand` `dream`).
fn run_command(session: &SessionDream, args: &str) -> Result<FeatureCommandOutcome, String> {
    let service = &session.feature.service;
    match parse_dream_command(args)? {
        DreamCommand::Run(request) => {
            let task = request.task;
            let facts = run_facts(&request, "command");
            let launch = session
                .feature
                .launch(service.start(request, trigger()), facts)?;
            Ok(FeatureCommandOutcome {
                text: format!("Dream-RSI run {} started: {}", launch.run_id, task.as_str()),
                completion: Some(completion_future("run", launch)),
            })
        }
        DreamCommand::Experiment(request) => {
            let task = request.task;
            let arms: Vec<&str> = request.arms.as_ref().map_or_else(
                || vec!["dream", "fixed"],
                |arms| arms.iter().map(|arm| arm.as_str()).collect(),
            );
            let arms = arms.join(",");
            let seeds = request
                .seeds
                .as_ref()
                .map_or_else(String::new, |seeds| format!(", {} seeds", seeds.len()));
            let facts = experiment_facts(&request, "command");
            let launch = session
                .feature
                .launch(service.start_experiment(request, trigger()), facts)?;
            Ok(FeatureCommandOutcome {
                text: format!(
                    "Dream-RSI experiment {} started: {} ({arms}{seeds})",
                    launch.run_id,
                    task.as_str()
                ),
                completion: Some(completion_future("experiment", launch)),
            })
        }
    }
}

impl SessionFeature for DreamFeature {
    fn name(&self) -> &'static str {
        "dream"
    }

    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        if !dream_allowed(context) {
            return;
        }
        let context = Arc::new(context.clone());
        let Some(session) = self.session(&context) else {
            return;
        };
        let session = Arc::new(session);
        for request_type in DREAM_REQUEST_TYPES {
            let session = Arc::clone(&session);
            handlers.register(
                request_type,
                host_handler(move |payload| {
                    let session = Arc::clone(&session);
                    // The launch reads the turn's trace context here, on
                    // the handler's own task.
                    let reply = handle(&session, request_type, &payload.data);
                    async move { reply.map_err(|message| anyhow::anyhow!(message)) }
                }),
            );
        }
    }

    fn slash_commands(&self) -> Vec<BuiltinSlashCommand> {
        vec![BuiltinSlashCommand {
            name: DREAM_COMMAND,
            description: DREAM_COMMAND_DESCRIPTION,
            execution: SlashCommandExecution::Session,
            argument_hint: Some(DREAM_COMMAND_HINT),
            aliases: &[],
            takes_argument: true,
        }]
    }

    fn execute_slash_command(
        &self,
        context: &Arc<SessionFeatureContext>,
        name: &str,
        args: &str,
    ) -> Option<FeatureFuture<Result<FeatureCommandOutcome, String>>> {
        if name != DREAM_COMMAND {
            return None;
        }
        let outcome = if dream_allowed(context) {
            self.session(context)
                .ok_or_else(|| UNAVAILABLE.to_string())
                .and_then(|session| run_command(&session, args))
        } else {
            Err(UNAVAILABLE.to_string())
        };
        Some(Box::pin(async move { outcome }))
    }

    fn bundled_skills(&self) -> Vec<&'static str> {
        vec![DREAM_SKILL]
    }

    fn flush(&self, _deadline: std::time::Instant) {
        // The process is exiting: stop every run at its next boundary.
        for service in self
            .services
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            service.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::needless_pass_by_value)] // takes the json! literal
    fn status(value: Value) -> DreamRunStatus {
        let mut base = json!({
            "runId": "dream_1", "phase": "rollout", "task": "circle-packing", "iteration": 1,
            "bestNodeScore": 0.5, "startedAt": 1, "updatedAt": 2, "kind": "run"
        });
        for (key, value) in value.as_object().unwrap() {
            base[key] = value.clone();
        }
        crate::run_service::status_from_value(&base).unwrap()
    }

    #[test]
    fn the_status_line_matches_the_ts_formatter() {
        assert_eq!(
            dream_status_line(&status(json!({}))),
            "dream rollout it1 best 0.5000"
        );
        assert_eq!(
            dream_status_line(&status(
                json!({"finalPolicyScore": 0.25, "improved": true, "phase": "accepted"})
            )),
            "dream accepted it1 best 0.5000 final 0.2500 improved true"
        );
        assert_eq!(
            dream_status_line(&status(
                json!({"kind": "experiment", "seedIndex": 0, "seedCount": 3, "arm": "fixed", "armIndex": 1, "armCount": 2, "round": 2, "rounds": 4, "tokens": 120})
            )),
            "dream experiment seed 1/3 fixed 2/2 r2/4 rollout best 0.5000 tokens 120"
        );
        assert_eq!(
            dream_status_line(&status(
                json!({"kind": "experiment", "stopReason": "completed", "resultPaths": ["a", "b"]})
            )),
            "dream experiment completed (2 result files)"
        );
        assert_eq!(
            dream_status_line(&status(json!({"error": "boom", "phase": "stopped"}))),
            "dream error"
        );
    }

    #[test]
    fn the_terminal_row_names_the_kind_the_run_the_reason_and_every_result() {
        let done = status(
            json!({"kind": "experiment", "stopReason": "completed", "resultPaths": ["/x/a.json", "/x/b.json"]}),
        );
        assert_eq!(
            completion_text("experiment", "dream_1", Ok(done)),
            Ok("Dream-RSI experiment dream_1 completed (results /x/a.json, /x/b.json)".to_string())
        );
        assert_eq!(
            completion_text(
                "run",
                "dream_1",
                Ok(status(json!({"stopReason": "cancelled"})))
            ),
            Ok("Dream-RSI run dream_1 cancelled".to_string())
        );
        assert_eq!(
            completion_text("run", "dream_1", Err("boom".to_string())),
            Err("Dream-RSI run dream_1 failed: boom".to_string())
        );
    }
}
