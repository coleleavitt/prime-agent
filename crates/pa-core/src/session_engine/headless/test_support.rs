//! Test fixtures shared by the headless tests: a daemonless engine over a tempdir, driven by the
//! process-global faux provider.

use std::path::Path;

use pa_types::ai::Model;
use serde_json::Value;

use crate::session::manager::SessionManager;
use crate::session_engine::engine::{SessionEngine, SessionEngineConfig, create_session};
use crate::session_engine::provider_adapter::{json_round_trip, real_stream_fn};
use crate::session_engine::telemetry::TelemetryWiring;

/// Writes `settings` as `<dir>/agent/settings.json` and registers the faux provider from
/// `script`; returns the scripted model.
pub(super) fn register_faux_model(dir: &Path, script: &Value, settings: &Value) -> Model {
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
    let parsed = pa_ai::faux::script::parse_faux_script(script).unwrap();
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            models: Some(vec![parsed.model]),
            ..Default::default()
        });
    registration.set_responses(parsed.responses);
    registration.set_repeat_last_response(parsed.repeat_last_response);
    registration.get_model()
}

/// A daemonless session engine over `dir` (agent dir `<dir>/agent`) that streams from the faux
/// `model`.
pub(super) async fn faux_engine(
    dir: &Path,
    model: &Model,
    session_manager: SessionManager,
    telemetry: Option<TelemetryWiring>,
) -> SessionEngine {
    let agent_model: pa_agent::types::Model =
        json_round_trip(model).expect("the faux model crosses the loop boundary");
    create_session(SessionEngineConfig {
        sandbox_mode: None,
        additional_directories: Vec::new(),
        plan_mode: None,
        on_late_sent_agent_message: None,
        semantic_edges: None,
        cron_store: None,
        steering_mode: None,
        follow_up_mode: None,
        image_model_router: None,
        telemetry,
        cwd: dir.to_path_buf(),
        agent_dir: dir.join("agent"),
        mcp_manager: None,
        model: Some(agent_model),
        thinking_level: None,
        stream_fn: Some(real_stream_fn(None, model.clone())),
        tools: Vec::new(),
        custom_system_prompt: None,
        prompt_guidelines: Vec::new(),
        generic_mcp_servers: Vec::new(),
        allow_recursion: None,
        session_manager: Some(session_manager),
        extra_host_handlers: None,
        conversation_log_path: None,
        additional_skill_paths: Vec::new(),
        additional_prompt_paths: Vec::new(),
        resource_exclusions: pa_types::daemon::SessionResourceExclusions::default(),
        extra_builtin_skill_overrides: Vec::new(),
        rlm_subagent_host: None,
        rlm_usage_store: None,
        progress_note_emit: None,
        factory_notices: None,
        rlm_depth: None,
        factory_child: false,
        daemonless: true,
        model_info: Some(model.clone()),
        kernel_ready_gate: None,
        prewarm_ipython_kernel: None,
        on_background_work_settled: None,
        bash_notice_wake: None,
        queued_goal_context_purge: None,
        queued_steering_probe: None,
        steering_fold: None,
        should_stop_after_turn: None,
        agent_definition: None,
    })
    .await
    .expect("the faux session assembles")
}
