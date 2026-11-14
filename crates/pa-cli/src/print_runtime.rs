//! The headless print runtime: single-shot prompt -> answer over the pa-core
//! session engine with a real pa-ai provider.

use std::sync::Arc;

use pa_agent::types::Model as AgentModel;
use pa_core::session::discovery::{
    ResolvedSession,
    SessionSelectorError,
    find_most_recent_session_for_cwd,
    resolve_session_path,
};
use pa_core::session_engine::headless::{
    HeadlessModelFlags,
    PrintRun,
    PrintRunConfig,
    PrintRunOutcome,
    resolve_headless_thinking_level,
    select_headless_model,
};
use pa_core::session_engine::image_model_routing;
use pa_core::session_engine::provider_adapter::{
    ProviderTarget,
    json_round_trip,
    switchable_stream_fn,
};
use pa_core::session_engine::rlm_in_process::{
    ChildEngineOverrides,
    ChildLane,
    InlineBuiltEngine,
    InlineChildEngineBuilder,
    InlineRecipe,
    InlineSessionNode,
    adopt_built_engine,
    dispose_inline_subagents,
    inline_engine_wiring,
    inline_root_node,
};
use pa_core::session_engine::session_events::agent_event_json;
use pa_types::ai::Model;
use tokio::sync::Notify;

use crate::mode::{AppMode, MissingSubsystem, RunOptions};

pub struct PrintRuntime;

impl crate::mode::Runtime for PrintRuntime {
    fn run(&self, options: &RunOptions) -> Result<i32, MissingSubsystem> {
        if options.list_models.is_some() {
            return match crate::list_models::run(options) {
                Ok(code) => Ok(code),
                Err(message) => {
                    eprintln!("Error: {message}");
                    Ok(1)
                }
            };
        }
        match options.app_mode {
            // Runtime failures print themselves and exit non-zero; the typed
            // MissingSubsystem channel stays reserved for unwired subsystems.
            AppMode::Print | AppMode::Json => match run_print_mode(options) {
                Ok(code) => Ok(code),
                Err(message) => {
                    eprintln!("Error: {message}");
                    Ok(1)
                }
            },
            AppMode::Interactive => match crate::interactive_mode::run_interactive_mode(options) {
                Ok(code) => Ok(code),
                Err(error) => {
                    eprintln!("Error: {error:#}");
                    Ok(1)
                }
            },
            AppMode::Daemon => {
                match crate::daemon_mode::run_daemon_mode(
                    options.daemon_socket.as_deref(),
                    &crate::daemon_mode::DaemonTcpFlags {
                        port: options.daemon_port,
                        bind_host: options.daemon_bind_host.clone(),
                    },
                ) {
                    Ok(code) => Ok(code),
                    Err(error) => {
                        eprintln!("Error: {error:#}");
                        Ok(1)
                    }
                }
            }
            // ACP mode: a thin JSON-RPC stdio transport over a daemon
            // session.
            AppMode::Acp => match run_acp_mode(options) {
                Ok(code) => Ok(code),
                Err(error) => {
                    eprintln!("Error: {error:#}");
                    Ok(1)
                }
            },
            AppMode::Rpc => match run_rpc_mode(options) {
                Ok(code) => Ok(code),
                Err(error) => {
                    eprintln!("Error: {error:#}");
                    Ok(1)
                }
            },
        }
    }
}

/// The autonomous runtime config from the typed CLI flags: any
/// autonomous flag enables the run.
fn autonomous_runtime_config(
    config: &crate::args::AutonomousConfig,
) -> pa_core::autonomous::AgentAutonomousConfig {
    pa_core::autonomous::AgentAutonomousConfig {
        enabled: Some(true),
        max_continuations: config.max_continuations.map(u64::from),
        max_turns: config.max_turns.map(u64::from),
        max_tokens: config.max_tokens,
        timeout_ms: config.timeout_ms,
        continuation_prompt: None,
        gates: config
            .gates
            .as_ref()
            .map(|gates| pa_core::autonomous::AgentAutonomousGateConfig {
                commands: Some(gates.commands.clone()),
                max_retries: gates.max_retries.map(u64::from),
                timeout_ms: gates.timeout_ms,
            }),
        subagent_keep_alive_ms: None,
    }
}

/// The ACP headless mode (TS main.ts, `useDaemonClient`): ensure a
/// supervisor is listening (spawning one detached), create the daemon
/// session the CLI session flags select, and serve the ACP surface over it
/// until the client disconnects. Any startup failure is an `Error:` exit 1
/// before the first ACP frame.
fn run_acp_mode(options: &RunOptions) -> Result<i32, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    rt.block_on(acp_mode_main(options))
}

async fn acp_mode_main(options: &RunOptions) -> Result<i32, String> {
    // The disclosure prints on the ACP client's stderr before any
    // transport starts (TS pushes the daemon-created session's diagnostics
    // to the client; the Rust stderr surface prints it here).
    crate::telemetry_notice::print_if_due(&options.config);
    // Flag > env > default: the same `PRIME_AGENT_DAEMON_SOCKET` contract
    // as every other mode (the `prime-agent` launcher written by
    // install-rust.sh pins that env, so the ACP path must honor it or it
    // would target the TypeScript default socket and treat the schema
    // mismatch as a stale daemon).
    let socket_path = crate::config::resolve_daemon_socket_path(options.daemon_socket.as_deref());
    crate::interactive_mode::ensure_daemon_running(&socket_path, &options.config.cwd)
        .await
        .map_err(|error| format!("{error:#}"))?;
    let (actual_cwd, create) = daemon_acp_create(options)?;
    // The transport's own client (`acp session load`), built like every
    // other process's; the opt-out builds none.
    let telemetry = {
        let settings = pa_core::settings::SettingsManager::create(
            &options.config.cwd,
            &options.config.agent_dir,
        );
        (!crate::mode::telemetry_disabled(&settings)).then(|| {
            pa_core::session_engine::telemetry::build_client(&settings, &options.config.agent_dir)
        })
    };
    let result = pa_daemon::acp::daemon::run_daemon_attached_acp_mode(
        pa_daemon::acp::daemon::DaemonAcpOptions {
            socket_path,
            actual_cwd,
            product_version: crate::config::version().to_string(),
            create,
            telemetry: telemetry.clone(),
        },
    )
    .await
    .map_err(|error| format!("{error:#}"));
    if let Some(client) = telemetry {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), client.shutdown()).await;
    }
    result
}

/// The ACP daemon session's create (TS main.ts `defaultSessionConfig` +
/// the startup create): the session the flags select, client-owned only
/// for `--no-session`, plus the session's cwd. The worker opens and
/// leases the selected file itself.
fn daemon_acp_create(
    options: &RunOptions,
) -> Result<(std::path::PathBuf, pa_types::daemon::DaemonCommand), String> {
    use pa_types::daemon::DaemonSessionLifecycle;
    let config = &options.config;
    let session_dir = replacement_session_dir(options);
    let (cwd, session_path, lifecycle) = if options.session.no_session {
        (
            config.cwd.clone(),
            None,
            DaemonSessionLifecycle::ClientOwned,
        )
    } else {
        let (cwd, session_path) = match select_headless_session(options)? {
            HeadlessSession::Fork(source) => {
                let fork = pa_core::session::manager::SessionManager::fork_from(
                    &source,
                    &config.cwd,
                    &session_dir,
                )?;
                (
                    config.cwd.clone(),
                    fork.get_session_file().map(std::path::Path::to_path_buf),
                )
            }
            HeadlessSession::Open(path) => (
                stored_session_cwd(&path, &config.cwd, explicit_cwd_override(options))?,
                Some(path),
            ),
            HeadlessSession::Fresh => (config.cwd.clone(), None),
        };
        (cwd, session_path, DaemonSessionLifecycle::Resident)
    };
    // The CLI session flags, under the TS `runtimeConfigFromArgs`
    // names. `--api-key` stays off: the create config is persisted.
    let mut create_config = serde_json::json!({
        "cwd": cwd.display().to_string(),
        "sessionDir": session_dir.display().to_string(),
        // The telemetry execution mode (TS main.ts `executionMode: appMode`).
        "executionMode": "acp",
    });
    if let Some(provider) = &config.provider {
        create_config["provider"] = serde_json::json!(provider);
    }
    if let Some(model) = &config.model {
        create_config["model"] = serde_json::json!(model);
    }
    if let Some(thinking) = config.thinking {
        create_config["thinking"] = serde_json::json!(thinking.wire_name());
    }
    if let Some(system_prompt) = &config.system_prompt {
        create_config["systemPrompt"] = serde_json::json!(system_prompt);
    }
    if !config.append_system_prompt.is_empty() {
        create_config["appendSystemPrompt"] = serde_json::json!(config.append_system_prompt);
    }
    for (key, paths) in [
        ("skills", &config.skills),
        ("promptTemplates", &config.prompt_templates),
    ] {
        if !paths.is_empty() {
            create_config[key] = paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .into();
        }
    }
    if let (Some(object), Ok(serde_json::Value::Object(exclusions))) = (
        create_config.as_object_mut(),
        serde_json::to_value(config.resource_exclusions()),
    ) {
        object.extend(exclusions);
    }
    if let Some(autonomous) = &config.autonomous {
        create_config["autonomous"] = serde_json::json!(autonomous_runtime_config(autonomous));
    }
    // Verification seam (the interactive mode's contract): a scripted daemon
    // session from a script FILE path. The product never sets it.
    if let Some(script) = std::env::var_os("PRIME_AGENT_FAUX_SCRIPT") {
        create_config["script"] = serde_json::Value::String(script.to_string_lossy().to_string());
    }
    // Verification seam (the TS child runtime inherits the parent's
    // `sessionConfig`): a scripted parent session's children run this script
    // FILE. The product never sets it.
    if let Some(child_script) = std::env::var_os("PRIME_AGENT_FAUX_CHILD_SCRIPT") {
        create_config["childScript"] =
            serde_json::Value::String(child_script.to_string_lossy().to_string());
    }
    // TS main.ts `launchEnv` (:1092 via the daemon-client create): the
    // client's own environment travels on the create so the session's
    // worker runs in the environment of the terminal that invoked it,
    // not the daemon's boot environment.
    let launch_env = {
        let env = pa_types::daemon::launch_env::collect_process_launch_env();
        (!env.is_empty()).then_some(env)
    };
    let create = pa_types::daemon::DaemonCommand::Create {
        id: None,
        session_path: session_path.map(|path| path.display().to_string()),
        continue_recent: None,
        no_session: options.session.no_session.then_some(true),
        name: None,
        config: Some(create_config),
        telemetry_disabled: crate::mode::create_telemetry_disabled(config),
        runtime_metadata: None,
        lifecycle: Some(lifecycle),
        env: None,
        launch_env,
        rest: serde_json::Map::default(),
    };
    Ok((cwd, create))
}

fn run_rpc_mode(options: &RunOptions) -> Result<i32, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    rt.block_on(rpc_mode_main(options))
}

async fn rpc_mode_main(options: &RunOptions) -> Result<i32, String> {
    let config = &options.config;
    let mut initial_lease = None;
    let parts = build_headless_engine_parts(options, "rpc", &mut initial_lease).await?;
    if let Some(goal) = &config.initial_goal {
        parts
            .engine
            .seed_initial_goal(&goal.objective, goal.token_budget.map(u64::from))
            .await
            .map_err(|error| format!("{error:#}"))?;
    }
    let factory = rpc_engine_factory(options);
    let mut engine_handle = pa_daemon::rpc::session::RpcEngineHandle::from(parts);
    engine_handle.session_lease = initial_lease;
    let exit_code = pa_daemon::rpc::run_rpc_mode(pa_daemon::rpc::RpcOptions {
        engine: engine_handle,
        engine_factory: Some(factory),
        cwd: config.cwd.clone(),
        agent_dir: config.agent_dir.clone(),
        autonomous_config: options
            .config
            .autonomous
            .as_ref()
            .map(autonomous_runtime_config),
    })
    .await
    .map_err(|error| format!("{error:#}"))?;
    Ok(exit_code)
}

/// The engine-replacement seam the RPC mode's `new_session`/`switch_session`/
/// `fork` commands drive: pa-cli owns the assembly, the mode owns the swap.
fn rpc_engine_factory(options: &RunOptions) -> pa_daemon::rpc::session::RpcEngineFactory {
    let options = options.clone();
    std::sync::Arc::new(move |request| {
        let mut options = options.clone();
        // The replacement sessions ignore the CLI's session-selection flags.
        options.session.resume = None;
        options.session.resume_bare = false;
        options.session.continue_recent = false;
        options.session.fork = None;
        Box::pin(async move {
            let (manager, opened_lease) = match &request {
                pa_daemon::rpc::session::RpcEngineRequest::New {
                    parent_session,
                    cwd,
                } => {
                    let session_dir = replacement_session_dir(&options);
                    // The active session's cwd when the command passed one, else the CLI startup
                    // directory.
                    let cwd = cwd.clone().unwrap_or_else(|| options.config.cwd.clone());
                    let manager = match parent_session {
                        Some(parent) => {
                            let mut manager = pa_core::session::manager::SessionManager::persisted(
                                &cwd,
                                &session_dir,
                            );
                            manager.new_session(&pa_core::session::manager::NewSessionOptions {
                                parent_session: Some(parent.clone()),
                                ..Default::default()
                            });
                            manager
                        }
                        None => {
                            pa_core::session::manager::SessionManager::persisted(&cwd, &session_dir)
                        }
                    };
                    // The fresh session's file is leased BEFORE the replacement can write it.
                    let lease = pa_daemon::lease::acquire_runtime_session_lease(
                        manager
                            .get_session_file()
                            .expect("a fresh session knows its file"),
                        &options.config.agent_dir,
                    )
                    .map_err(|error| format!("{error:#}"))?;
                    (manager, Some(lease))
                }
                pa_daemon::rpc::session::RpcEngineRequest::Open {
                    session_path,
                    reuse_lease,
                    ..
                } => {
                    let session_dir = replacement_session_dir(&options);
                    let cwd = options.config.cwd.clone();
                    // A same-path reopen skips the guard (the session layer adopted it).
                    let lease = if *reuse_lease {
                        None
                    } else {
                        Some(session_open_guard(
                            options.daemon_socket.as_deref(),
                            session_path,
                        )?)
                    };
                    let manager = open_session_file(session_path, &session_dir, &cwd, None)?;
                    // The replacement ADOPTS the opened session's own cwd: tools,
                    // settings, and file work run against the session's repository.
                    options.config.cwd = manager.get_cwd().to_path_buf();
                    (manager, lease)
                }
            };
            let kernel_ready_gate = match &request {
                pa_daemon::rpc::session::RpcEngineRequest::Open { ready_gate, .. } => {
                    ready_gate.clone()
                }
                pa_daemon::rpc::session::RpcEngineRequest::New { .. } => None,
            };
            let engine = build_engine(
                &options,
                faux_script_env()?.as_ref(),
                Some(manager),
                "rpc",
                &HeadlessEmbedding::Rpc { kernel_ready_gate },
            )
            .await?;
            let mut handle = pa_daemon::rpc::session::RpcEngineHandle::from(engine);
            handle.session_lease = opened_lease;
            Ok(handle)
        })
    })
}

fn replacement_session_dir(options: &RunOptions) -> std::path::PathBuf {
    options
        .session
        .session_dir
        .clone()
        .unwrap_or_else(|| options.config.agent_dir.join("sessions"))
}

impl From<HeadlessEngine> for pa_daemon::rpc::session::RpcEngineHandle {
    fn from(parts: HeadlessEngine) -> Self {
        Self {
            engine: std::sync::Arc::new(parts.engine),
            model: parts.model,
            api_key: parts.api_key,
            provider_target: parts.provider_target,
            bash_wake: parts
                .rpc_bash_wake
                .expect("the RPC embedding builds its background-bash wake handle"),
            session_lease: None,
        }
    }
}

fn run_print_mode(options: &RunOptions) -> Result<i32, String> {
    with_print_runtime(options, |options, lease| {
        Box::pin(print_mode_main(options, lease))
    })
}

/// Own the print lease through runtime shutdown, including errors and unwinding.
/// The operation seam lets the regression exercise a writer pending at shutdown.
fn with_print_runtime<Context>(
    context: &Context,
    run: impl for<'a> FnOnce(
        &'a Context,
        &'a mut Option<pa_daemon::lease::SessionLease>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<i32, String>> + 'a>,
    >,
) -> Result<i32, String> {
    // Declared first so unwinding also stops all runtime tasks before release.
    let mut lease = None;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let result = rt.block_on(run(context, &mut lease));
    // Runtime Drop joins blocking writers and cancels async host tasks. Engine
    // Drop alone cannot do that: detached host requests retain session writers.
    drop(rt);
    result
}

async fn print_mode_main(
    options: &RunOptions,
    lease: &mut Option<pa_daemon::lease::SessionLease>,
) -> Result<i32, String> {
    // `print` or `json`: the telemetry execution mode is the app mode (TS
    // main.ts `executionMode: appMode`).
    crate::telemetry_notice::print_if_due(&options.config);
    let (session_manager, opened_lease) = select_session_manager_with_lease(options)?;
    *lease = opened_lease;
    let session_manager = session_manager.unwrap_or_else(|| {
        pa_core::session::manager::SessionManager::in_memory(&options.config.cwd)
    });
    // The print root's inline-subagent node: the quiescence drive, the
    // host, and the messaging controller all hang off it. The faux
    // script (when scripted) rides the recipe down to every child, and
    // every child's engine builds through this same assembly.
    let faux = faux_script_env()?;
    let root_node = inline_root_node(
        &session_manager,
        InlineRecipe {
            cwd: options.config.cwd.clone(),
            agent_dir: options.config.agent_dir.clone(),
            faux: faux.clone(),
            build_child: inline_child_engine_builder(options, faux.clone()),
        },
    );
    let headless = build_engine(
        options,
        faux.as_ref(),
        Some(session_manager),
        options.app_mode.as_str(),
        &HeadlessEmbedding::Root {
            node: std::sync::Arc::clone(&root_node),
        },
    )
    .await?;
    let engine = std::sync::Arc::new(headless.engine);
    adopt_built_engine(&root_node, &engine, &headless.model).await;
    // The CLI `--goal` seed: a fresh root branch starts the goal; a resumed
    // branch keeps its persisted goal. Depth 0 only — the print session is a root.
    if let Some(goal) = &options.config.initial_goal {
        engine
            .seed_initial_goal(&goal.objective, goal.token_budget.map(u64::from))
            .await
            .map_err(|error| format!("{error:#}"))?;
    }
    let exit = run_prompts_and_emit(
        &engine,
        &headless.model,
        headless.api_key.clone(),
        options,
        &root_node,
    )
    .await;
    // No inline descendant outlives the print process: kernels get their
    // final snapshot, drivers join, runs abort (every exit path).
    dispose_inline_subagents(&root_node).await;
    exit
}

/// Every inline child's engine: the invocation's own assembly at the
/// child's cwd, scripted when the run is.
fn inline_child_engine_builder(
    options: &RunOptions,
    faux: Option<serde_json::Value>,
) -> InlineChildEngineBuilder {
    let options = options.clone();
    std::sync::Arc::new(move |build| {
        let mut options = options.clone();
        options.config.cwd = build.cwd;
        let faux = faux.clone();
        Box::pin(async move {
            let headless = build_engine(
                &options,
                faux.as_ref(),
                Some(build.session_manager),
                "subagent",
                &HeadlessEmbedding::Child(Box::new(build.overrides)),
            )
            .await?;
            Ok(InlineBuiltEngine {
                engine: headless.engine,
                model: headless.model,
                api_key: headless.api_key,
            })
        })
    })
}

/// One engine build for every headless embedding: the scripted
/// verification seam when a faux script is set, the real provider
/// otherwise.
pub(crate) async fn build_engine(
    options: &RunOptions,
    faux: Option<&serde_json::Value>,
    session_manager: Option<pa_core::session::manager::SessionManager>,
    execution_mode: &str,
    embedding: &HeadlessEmbedding,
) -> Result<HeadlessEngine, String> {
    match faux {
        Some(script) => {
            build_faux_engine_with(options, script, session_manager, execution_mode, embedding)
                .await
        }
        None => {
            build_headless_engine_with(options, session_manager, execution_mode, embedding).await
        }
    }
}

/// The faux-script verification seam (`PRIME_AGENT_FAUX_SCRIPT`; never
/// set by the product): the script env parse and the scripted engine
/// build.
mod faux;

pub(crate) use faux::{build_faux_engine_with, faux_script_env};

/// The switchable provider target the session's stream reads per call
/// (shared with the RPC mode, whose picker model switches swap it live).
pub type ProviderTargetSlot = std::sync::Arc<std::sync::RwLock<Option<ProviderTarget>>>;

/// What one engine build varies by session: the print root (inline
/// subagents over its node), a spawned inline child (its own model,
/// auth, and session), or the RPC mode (no inline subagents — today's
/// behavior).
pub(crate) enum HeadlessEmbedding {
    Root {
        node: std::sync::Arc<InlineSessionNode>,
    },
    Child(Box<ChildEngineOverrides>),
    Rpc {
        /// A same-path reopen's gate: the replaced kernel's final snapshot flush.
        kernel_ready_gate: Option<pa_core::kernel::provisioner::KernelReadyGate>,
    },
}

/// The per-embedding fields both headless builders (the real provider
/// and the faux script) share: the inline host + agent-message handlers
/// with the RLM depth, the background-bash wake both engine seams fire
/// (the completion row's admission wake and the kernel's
/// background-settle callback), and the late agent-message receipt sink
/// print sessions record through.
pub(crate) struct HeadlessEmbeddingFields {
    pub(crate) rlm_subagent_host:
        Option<Arc<dyn pa_core::session_engine::rlm_host::RlmSubagentHost>>,
    pub(crate) extra_host_handlers: Option<pa_core::kernel::shared::HostRequestHandlers>,
    pub(crate) rlm_depth: Option<u32>,
    pub(crate) factory_child: bool,
    pub(crate) bash_wake: pa_core::session_engine::bash_notices::BashNoticeWake,
    /// The RPC mode's wake handle, carried onto the engine handle so the
    /// queue-pump listener parks on it; `None` outside the RPC embedding.
    pub(crate) rpc_bash_wake: Option<Arc<Notify>>,
    /// The late fire-and-forget `agent_message.send` receipt sink
    /// (#10-print); `None` for the RPC embedding (no print node).
    pub(crate) on_late_sent_agent_message: Option<pa_core::LateSentAgentMessageHandler>,
    pub(crate) kernel_ready_gate: Option<pa_core::kernel::provisioner::KernelReadyGate>,
}

/// The embedding-owned fields for one headless engine build: print
/// sessions take the inline node's wiring (every quiescence park listens
/// on its activity), the RPC mode wakes the built handle's notify.
pub(crate) fn headless_embedding_fields(embedding: &HeadlessEmbedding) -> HeadlessEmbeddingFields {
    let node = match embedding {
        HeadlessEmbedding::Root { node } => node,
        HeadlessEmbedding::Child(overrides) => &overrides.node,
        HeadlessEmbedding::Rpc { kernel_ready_gate } => {
            let notify = Arc::new(Notify::new());
            let wake = {
                let notify = Arc::clone(&notify);
                Arc::new(move || notify.notify_waiters())
                    as pa_core::session_engine::bash_notices::BashNoticeWake
            };
            return HeadlessEmbeddingFields {
                rlm_subagent_host: None,
                extra_host_handlers: None,
                rlm_depth: None,
                factory_child: false,
                bash_wake: wake,
                rpc_bash_wake: Some(notify),
                on_late_sent_agent_message: None,
                kernel_ready_gate: kernel_ready_gate.clone(),
            };
        }
    };
    let wiring = inline_engine_wiring(node);
    HeadlessEmbeddingFields {
        rlm_subagent_host: Some(wiring.rlm_subagent_host),
        extra_host_handlers: Some(wiring.extra_host_handlers),
        rlm_depth: wiring.rlm_depth,
        factory_child: matches!(
            embedding,
            HeadlessEmbedding::Child(overrides) if overrides.factory_child
        ),
        bash_wake: wiring.bash_wake,
        rpc_bash_wake: None,
        on_late_sent_agent_message: Some(wiring.on_late_sent_agent_message),
        kernel_ready_gate: None,
    }
}

/// The assembled headless engine, so host transports can drive session-command
/// executors (compact/refine) with the session's own model.
pub(crate) struct HeadlessEngine {
    pub(crate) engine: pa_core::session_engine::engine::SessionEngine,
    pub(crate) model: Model,
    pub(crate) api_key: Option<String>,
    /// The live provider target: the stream reads it per call, and the
    /// RPC mode's picker switches swap it (TS `setModel`'s stream
    /// re-registration; `set_model` swaps it without rebuilding the
    /// session).
    provider_target: ProviderTargetSlot,
    /// The RPC mode's background-bash wake handle; `None` for the print
    /// embeddings (their wake is the node's activity notify).
    pub(crate) rpc_bash_wake: Option<Arc<Notify>>,
}

/// Transfer the opened lease to its mode before fallible engine assembly.
async fn build_headless_engine_parts(
    options: &RunOptions,
    execution_mode: &str,
    lease: &mut Option<pa_daemon::lease::SessionLease>,
) -> Result<HeadlessEngine, String> {
    // Every headless mode discloses immediately (TS main: only the
    // interactive `deferTelemetryNoticeForOnboarding` holds the notice
    // back behind onboarding; `--list-models` never reaches this
    // assembly, matching the TS exit before its diagnostics report).
    crate::telemetry_notice::print_if_due(&options.config);
    let (session_manager, opened_lease) = select_session_manager_with_lease(options)?;
    *lease = opened_lease;
    build_engine(
        options,
        faux_script_env()?.as_ref(),
        session_manager,
        execution_mode,
        &HeadlessEmbedding::Rpc {
            kernel_ready_gate: None,
        },
    )
    .await
}

/// The session-manager selection every engine build shares, returning the opened session's runtime
/// lease.
fn select_session_manager_with_lease(
    options: &RunOptions,
) -> Result<
    (
        Option<pa_core::session::manager::SessionManager>,
        Option<pa_daemon::lease::SessionLease>,
    ),
    String,
> {
    if options.session.no_session {
        return Ok((None, None));
    }
    let (manager, lease) = build_session_manager_with_lease(options)?;
    Ok((Some(manager), lease))
}

/// The real-provider engine assembly over one session-manager selection:
/// model resolution, session persistence, and the engine facade.
pub(crate) async fn build_headless_engine_with(
    options: &RunOptions,
    session_manager: Option<pa_core::session::manager::SessionManager>,
    execution_mode: &str,
    embedding: &HeadlessEmbedding,
) -> Result<HeadlessEngine, String> {
    let config = &options.config;

    // Model registry: composed catalog + models.json with real auth (the
    // Prime Inference team and key follow the session directory). A
    // spawned child carries its already-resolved model and request auth
    // (the spawn resolved them against this same catalog).
    let mut registry = pa_core::models::ModelRegistry::for_session(&config.agent_dir, &config.cwd);
    registry.load_private_authorization_from_cache();
    let model = match embedding {
        HeadlessEmbedding::Child(overrides) => match &overrides.lane {
            ChildLane::Real(model) => (**model).clone(),
            ChildLane::Faux(_) => {
                return Err("a scripted child builds through the faux builder".to_string());
            }
        },
        HeadlessEmbedding::Root { .. } | HeadlessEmbedding::Rpc { .. } => {
            // The opened session's active context joins the selection (TS
            // `createAgentSession`); `--no-session` selects from no session.
            let session = session_manager
                .as_ref()
                .map(pa_core::session::manager::SessionManager::active_context);
            select_headless_model(&mut registry, &model_flags(config), session.as_ref()).await?
        }
    };
    // A broken prime CLI directory context fails a Prime Inference run
    // instead of billing the stored login (the daemon turn preflight's
    // rule; the prime CLI refuses to run under it too).
    if model.provider == pa_core::auth::PRIME_INFERENCE_PROVIDER_ID {
        pa_core::auth::AuthStorage::for_session(&config.agent_dir, &config.cwd)
            .prime_directory_selection()
            .map_err(|error| {
                format!(
                    "Invalid Prime team selection: {error}\n\nFix it, or run `prime config unpin` in this directory."
                )
            })?;
    }
    // Resolve request auth once: the stored team / `PRIME_TEAM_ID` reach the wire through these
    // headers (children resolve against the same catalog the spawn used).
    let resolved = registry.get_api_key_and_headers(&model, model.headers.as_ref());

    let provider_target: ProviderTargetSlot =
        std::sync::Arc::new(std::sync::RwLock::new(Some(ProviderTarget {
            api_key: resolved.api_key.clone(),
            model: model.clone(),
            service_tier: None,
            headers: resolved.headers.clone(),
        })));
    let agent_model: AgentModel = json_round_trip(&model).ok_or("model conversion failed")?;

    // Children serve their own provider target (the route-authoritative
    // stream would serve the parent's model, and children have no
    // image-model routing).
    let (stream_fn, image_model_router) = if matches!(embedding, HeadlessEmbedding::Child(_)) {
        (
            switchable_stream_fn(std::sync::Arc::clone(&provider_target)),
            None,
        )
    } else {
        // While an episode is armed the stream serves THIS target, so a concurrent
        // `set_model` picker write cannot redirect an in-flight routed turn.
        let armed_target: std::sync::Arc<std::sync::Mutex<Option<ProviderTarget>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let stream_fn = image_model_routing::route_authoritative_stream_fn(
            std::sync::Arc::clone(&provider_target),
            std::sync::Arc::clone(&armed_target),
        );
        // TS settings.imageModel routing: image-attaching batches on a session model
        // without image input route to the configured image model or refuse.
        (
            stream_fn,
            Some(image_model_routing::slot_image_model_router(
                &provider_target,
                armed_target,
                config.cwd.clone(),
                config.agent_dir.clone(),
                model.clone(),
            )),
        )
    };

    // Telemetry: the CLI's env/settings opt-out decides; depth 0 only, enforced by the engine.
    // A spawned child never reports (the engine's depth gate holds anyway).
    let telemetry = (!config.telemetry_disabled
        && matches!(
            embedding,
            HeadlessEmbedding::Root { .. } | HeadlessEmbedding::Rpc { .. }
        ))
    .then(|| {
        let settings = pa_core::settings::SettingsManager::create(&config.cwd, &config.agent_dir);
        pa_core::session_engine::telemetry::TelemetryWiring {
            client: pa_core::session_engine::telemetry::build_client(&settings, &config.agent_dir),
            execution_mode: Some(execution_mode.to_string()),
            now: None,
            telemetry_enabled: Some(
                pa_core::session_engine::telemetry::telemetry_enabled_switch(
                    &config.cwd,
                    &config.agent_dir,
                ),
            ),
        }
    });
    let queue_settings = pa_core::settings::SettingsManager::create(&config.cwd, &config.agent_dir);
    let queue_mode = |mode: pa_core::settings::QueueModeSetting| match mode {
        pa_core::settings::QueueModeSetting::All => pa_agent::agent::QueueMode::All,
        pa_core::settings::QueueModeSetting::OneAtATime => pa_agent::agent::QueueMode::OneAtATime,
    };
    let steering_mode = Some(queue_mode(queue_settings.get_steering_mode()));
    let follow_up_mode = Some(queue_mode(queue_settings.get_follow_up_mode()));
    // Auth construction blocks; run it off the async runtime like the engine's own gating does.
    let mcp_manager = {
        let cwd = config.cwd.clone();
        let agent_dir = config.agent_dir.clone();
        let manager = tokio::task::spawn_blocking(move || {
            crate::mcp_login::cli_mcp_manager(&cwd, &agent_dir)
        })
        .await
        .map_err(|error| format!("MCP manager construction failed: {error}"))?;
        std::sync::Arc::new(std::sync::Mutex::new(manager))
    };
    // TS print mode records too (`semanticEdgeLedgerPath`): the session
    // id names the ledger under the session artifact dir; `--no-session`
    // runs on the in-memory manager (no artifact dir), so the identity is
    // ledger-less but the request ids still go on the wire.
    let session_manager = session_manager
        .unwrap_or_else(|| pa_core::session::manager::SessionManager::in_memory(&config.cwd));
    let HeadlessEmbeddingFields {
        rlm_subagent_host,
        extra_host_handlers,
        rlm_depth,
        factory_child,
        bash_wake,
        rpc_bash_wake,
        on_late_sent_agent_message,
        kernel_ready_gate,
    } = headless_embedding_fields(embedding);
    let (semantic_edges, thinking_level) = match embedding {
        HeadlessEmbedding::Child(overrides) => (
            Some(
                pa_core::session_engine::semantic_edges::SemanticEdgeIdentity {
                    session_id: session_manager.get_session_id().to_string(),
                    // A spawned child's ledger lives in its rlm session dir.
                    ledger_path: pa_core::session_engine::semantic_edges::semantic_edge_ledger_path(
                        Some(&overrides.session_dir),
                        session_manager.get_session_artifact_dir().as_deref(),
                    ),
                    parent_session_id: overrides.semantic_parent_session_id.clone(),
                    spawned_by_request_id: overrides.spawned_by_request_id.clone(),
                },
            ),
            Some(pa_core::session_engine::provider_adapter::map_thinking_level(overrides.thinking)),
        ),
        HeadlessEmbedding::Root { .. } | HeadlessEmbedding::Rpc { .. } => (
            Some(
                pa_core::session_engine::semantic_edges::SemanticEdgeIdentity {
                    session_id: session_manager.get_session_id().to_string(),
                    ledger_path: pa_core::session_engine::semantic_edges::semantic_edge_ledger_path(
                        None,
                        session_manager.get_session_artifact_dir().as_deref(),
                    ),
                    parent_session_id: None,
                    spawned_by_request_id: None,
                },
            ),
            Some(resolve_headless_thinking_level(
                &model_flags(config),
                &model,
            )),
        ),
    };
    let engine = pa_core::session_engine::engine::create_session(
        pa_core::session_engine::engine::SessionEngineConfig {
            // `--sandbox`; without it the `sandbox` setting decides.
            sandbox_mode: config.sandbox_mode,
            additional_directories: config.additional_directories.clone(),
            // `--plan`; without it the session restores its own mode. A
            // spawned child starts in its parent's mode.
            plan_mode: match embedding {
                HeadlessEmbedding::Child(overrides) => Some(overrides.plan_mode),
                HeadlessEmbedding::Root { .. } | HeadlessEmbedding::Rpc { .. } => {
                    config.plan_mode.then_some(true)
                }
            },
            on_late_sent_agent_message,
            cron_store: None,
            semantic_edges,
            telemetry,
            steering_mode,
            follow_up_mode,
            cwd: config.cwd.clone(),
            agent_dir: config.agent_dir.clone(),
            mcp_manager: Some(mcp_manager),
            model: Some(agent_model),
            thinking_level,
            stream_fn: Some(stream_fn),
            tools: builtin_tools(&config.cwd),
            custom_system_prompt: config.system_prompt.clone(),
            prompt_guidelines: config.append_system_prompt.clone(),
            generic_mcp_servers: vec![],
            allow_recursion: None,
            session_manager: Some(session_manager),
            extra_host_handlers,
            conversation_log_path: None,
            additional_skill_paths: config
                .skills
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            additional_prompt_paths: config
                .prompt_templates
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            resource_exclusions: config.resource_exclusions(),
            extra_builtin_skill_overrides: vec![],
            rlm_subagent_host,
            rlm_usage_store: None,
            progress_note_emit: None,
            factory_notices: None,
            rlm_depth,
            factory_child,
            daemonless: true,
            model_info: Some(model.clone()),
            // The kernel boots in the background at creation; a spawned
            // child boots lazily on first use.
            prewarm_ipython_kernel: (!matches!(embedding, HeadlessEmbedding::Child(_)))
                .then_some(true),
            kernel_ready_gate,
            // The background-bash wake pair: the completion row's
            // admission fires it, and the kernel's background settle does
            // too, so the embedding's parked drive or pump re-checks.
            bash_notice_wake: Some(Arc::clone(&bash_wake)),
            on_background_work_settled: Some(bash_wake),
            queued_goal_context_purge: None,
            queued_steering_probe: None,
            steering_fold: None,
            should_stop_after_turn: None,
            image_model_router,
            agent_definition: match embedding {
                HeadlessEmbedding::Child(overrides) => overrides.agent_definition.clone(),
                HeadlessEmbedding::Root { .. } | HeadlessEmbedding::Rpc { .. } => None,
            },
        },
    )
    .await
    .map_err(|error| format!("{error:#}"))?;
    Ok(HeadlessEngine {
        engine,
        model,
        api_key: resolved.api_key,
        provider_target,
        rpc_bash_wake,
    })
}

/// The session header line: the session file's `type: "session"` entry in the
/// TS wire shape and field order.
async fn session_header_json(
    engine: &pa_core::session_engine::engine::SessionEngine,
) -> Option<String> {
    let persistence = engine.session.shared_persistence();
    let session = persistence.lock().await;
    let header = session.get_header()?;
    // The TS field order, optional fields only when present.
    let mut object = serde_json::Map::new();
    let field = |map: &mut serde_json::Map<String, serde_json::Value>,
                 key: &str,
                 value: Option<serde_json::Value>| {
        if let Some(value) = value {
            map.insert(key.to_string(), value);
        }
    };
    field(&mut object, "type", Some(serde_json::json!("session")));
    field(
        &mut object,
        "version",
        header.version.map(serde_json::Value::from),
    );
    field(&mut object, "id", Some(serde_json::json!(header.id)));
    field(
        &mut object,
        "timestamp",
        Some(serde_json::json!(header.timestamp)),
    );
    field(&mut object, "cwd", Some(serde_json::json!(header.cwd)));
    field(
        &mut object,
        "parentSession",
        header
            .parent_session
            .as_ref()
            .map(|parent| serde_json::json!(parent)),
    );
    field(
        &mut object,
        "rlmDepth",
        header.rlm_depth.map(serde_json::Value::from),
    );
    field(
        &mut object,
        "git",
        header
            .git
            .as_ref()
            .map(|git| serde_json::to_value(git).unwrap_or(serde_json::Value::Null)),
    );
    Some(serde_json::Value::Object(object).to_string())
}

/// The headless model flags of one invocation.
fn model_flags(config: &crate::mode::RuntimeConfig) -> HeadlessModelFlags {
    HeadlessModelFlags {
        cwd: config.cwd.clone(),
        agent_dir: config.agent_dir.clone(),
        provider: config.provider.clone(),
        model: config.model.clone(),
        thinking: config.thinking,
    }
}

/// The headless session selection, in the flag order of TS
/// `createSessionManager` (noSession -> fork -> resume -> continue ->
/// create). `--no-session` never reaches here: its callers skip the
/// selection.
enum HeadlessSession {
    /// Fork this source session into a fresh file.
    Fork(std::path::PathBuf),
    /// Open this existing session file.
    Open(std::path::PathBuf),
    Fresh,
}

/// main.ts `explicitCwdOverride`: with --cwd, the flag's directory wins
/// over the stored session cwd on resume.
fn explicit_cwd_override(options: &RunOptions) -> Option<&std::path::Path> {
    options
        .session
        .cwd_from_flag
        .then_some(options.config.cwd.as_path())
}

fn select_headless_session(options: &RunOptions) -> Result<HeadlessSession, String> {
    let cwd = &options.config.cwd;
    let session_dir = replacement_session_dir(options);
    // TS `createSessionManager`'s fork arm: every resolution shape forks —
    // a GLOBAL session is exactly what --fork is for (a different
    // project's session copied into this cwd) — with no daemon-active
    // guard: the copy writes a fresh file, never the hosted source.
    if let Some(selector) = &options.session.fork {
        // A leading `~` expands against the home dir (the resume
        // selector's convention; the interactive fork arm matches).
        let expanded = crate::config::expand_tilde_path(selector);
        let selector = expanded.to_string_lossy();
        let resolved = resolve_session_path(&selector, cwd, &session_dir)
            .map_err(|error| render_selector_error(&error))?;
        let source = match resolved {
            ResolvedSession::Path(path)
            | ResolvedSession::Local(path)
            | ResolvedSession::Global { path, .. } => path,
        };
        return Ok(HeadlessSession::Fork(source));
    }
    if let Some(selector) = &options.session.resume {
        let resolved = match resolve_session_path(selector, cwd, &session_dir) {
            Ok(resolved) => resolved,
            // An id miss tries the saved sessions' titles (the interactive resolve's order).
            Err(error @ SessionSelectorError::NotFound { .. }) => {
                let sessions = pa_daemon::session_store::list_sessions(&session_dir);
                match crate::resume_selector::sessions_titled(&sessions, selector, cwd).as_slice() {
                    // Another product's session (a Claude Code id or title) imports into
                    // a new session file first.
                    [] => {
                        match crate::resume_selector::import_external(selector, cwd, &session_dir) {
                            None => return Err(render_selector_error(&error)),
                            Some(Err(notice)) => return Err(notice),
                            Some(Ok(imported))
                                if pa_types::platform::same_directory(
                                    std::path::Path::new(&imported.cwd),
                                    cwd,
                                ) =>
                            {
                                ResolvedSession::Local(imported.path)
                            }
                            Some(Ok(imported)) => ResolvedSession::Global {
                                path: imported.path,
                                cwd: std::path::PathBuf::from(&imported.cwd),
                            },
                        }
                    }
                    [only]
                        if pa_types::platform::same_directory(
                            std::path::Path::new(&only.cwd),
                            cwd,
                        ) =>
                    {
                        ResolvedSession::Local(only.path.clone())
                    }
                    [only] => ResolvedSession::Global {
                        path: only.path.clone(),
                        cwd: std::path::PathBuf::from(&only.cwd),
                    },
                    several => {
                        return Err(format!(
                            "Ambiguous session title \"{selector}\": matches {}",
                            several
                                .iter()
                                .map(|info| info.id.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                }
            }
            Err(error) => return Err(render_selector_error(&error)),
        };
        return match resolved {
            ResolvedSession::Path(path) | ResolvedSession::Local(path) => {
                Ok(HeadlessSession::Open(
                    std::path::absolute(&path).map_err(|error| error.to_string())?,
                ))
            }
            ResolvedSession::Global {
                path: _,
                cwd: session_cwd,
            } => {
                // Headless modes have no fork prompt; mirror the TS non-TTY path.
                Err(format!(
                    "session {selector} belongs to a different project ({}). Pass --fork {selector} to use it here, or run from that project's directory.",
                    session_cwd.display()
                ))
            }
        };
    }
    if options.session.continue_recent {
        // Absolute like the resume arm (TS `setSessionFile` resolves it).
        if let Some(path) = find_most_recent_session_for_cwd(&session_dir, cwd) {
            return Ok(HeadlessSession::Open(
                std::path::absolute(&path).map_err(|error| error.to_string())?,
            ));
        }
    }
    Ok(HeadlessSession::Fresh)
}

/// The in-process session manager for the selected session. The opened
/// session's runtime lease returns alongside: the driving mode owns it for
/// its run's lifetime.
fn build_session_manager_with_lease(
    options: &RunOptions,
) -> Result<
    (
        pa_core::session::manager::SessionManager,
        Option<pa_daemon::lease::SessionLease>,
    ),
    String,
> {
    let cwd = options.config.cwd.clone();
    let session_dir = replacement_session_dir(options);
    match select_headless_session(options)? {
        HeadlessSession::Fork(source) => {
            let manager =
                pa_core::session::manager::SessionManager::fork_from(&source, &cwd, &session_dir)?;
            // The materialized fork leases its own file before the engine
            // writes it (the fresh-session rule): another process resuming
            // the new file can never become a second writer while this
            // engine appends — the source was only read, never leased.
            Ok(lease_fresh_manager(manager))
        }
        HeadlessSession::Open(path) => {
            let lease = session_open_guard(options.daemon_socket.as_deref(), &path)?;
            // A failed open's early return drops the lease (released),
            // never leaving an orphaned hold behind.
            let manager =
                open_session_file(&path, &session_dir, &cwd, explicit_cwd_override(options))?;
            Ok((manager, Some(lease)))
        }
        HeadlessSession::Fresh => Ok(fresh_session_with_lease(&cwd, &session_dir)),
    }
}

/// Build a FRESH persisted manager and lease its eagerly selected file before the engine can write
/// it.
fn fresh_session_with_lease(
    cwd: &std::path::Path,
    session_dir: &std::path::Path,
) -> (
    pa_core::session::manager::SessionManager,
    Option<pa_daemon::lease::SessionLease>,
) {
    let manager = pa_core::session::manager::SessionManager::persisted(cwd, session_dir);
    lease_fresh_manager(manager)
}

/// Lease a freshly materialized session file before the engine can write it: the
/// ungated runtime acquire (`Ok(None)` whenever the env gate is unset).
fn lease_fresh_manager(
    manager: pa_core::session::manager::SessionManager,
) -> (
    pa_core::session::manager::SessionManager,
    Option<pa_daemon::lease::SessionLease>,
) {
    let lease = match manager.get_session_file() {
        Some(path) => {
            match pa_daemon::lease::acquire_runtime_session_lease(
                path,
                &crate::config::get_agent_dir(),
            ) {
                Ok(lease) => Some(lease),
                Err(error) => {
                    eprintln!("prime-agent: could not lease the fresh session file: {error:#}");
                    None
                }
            }
        }
        None => None,
    };
    (manager, lease)
}

/// Guard an in-process open: probe the daemon's live roster, then acquire the
/// runtime lease. Returns the HELD lease — the caller owns its lifetime.
fn session_open_guard(
    socket_path: Option<&str>,
    session_path: &std::path::Path,
) -> Result<pa_daemon::lease::SessionLease, String> {
    let socket = crate::interactive_mode::resolve_socket_path(socket_path);
    if let Ok(mut client) = crate::daemon_client::DaemonClient::connect(&socket) {
        let list = client
            .request(pa_types::daemon::DaemonCommand::List {
                id: None,
                all: None,
                cwd: None,
                session_dir: None,
                include_client_owned: None,
                include_remote_mesh: None,
                rest: serde_json::Map::default(),
            })
            .map_err(|error| format!("Could not check active sessions: {error:#}"))?;
        if list.success {
            let target = pa_daemon::lease::canonical_session_path(session_path);
            for row in list
                .data
                .and_then(|data| data.get("sessions").cloned())
                .and_then(|sessions| sessions.as_array().cloned())
                .unwrap_or_default()
            {
                let Some(file) = row.get("sessionFile").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                if pa_daemon::lease::canonical_session_path(std::path::Path::new(file)) != target {
                    continue;
                }
                let active_session_id = row
                    .get("activeSessionId")
                    .or_else(|| row.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                // The descriptive refusal (operator-directed): attach to the live session instead
                // of reopening its file.
                let message = match pa_tui::session_open_error::holder_from_roster(
                    std::slice::from_ref(&row),
                    &target,
                ) {
                    Some(holder) => {
                        pa_tui::session_open_error::already_active_error(&holder, &target)
                    }
                    None => format!(
                        "Session is already active in {active_session_id}: {}",
                        target.display()
                    ),
                };
                return Err(message);
            }
        }
    }
    // The daemon's roster covers only its own sessions; the session store is
    // shared, so the lease table is the one cross-daemon ownership record.
    let agent_dir = crate::config::get_agent_dir();
    // Acquire, not observe: a probe leaves a window where another process acquires
    // between the check and this open — two writers on one file.
    match pa_daemon::lease::acquire_runtime_session_lease(session_path, &agent_dir) {
        Ok(lease) => Ok(lease),
        Err(error) => {
            let Some(active) = error.downcast_ref::<pa_daemon::lease::SessionAlreadyActiveError>()
            else {
                // The lease table itself failed (io, permissions): never silently proceed over an
                // undeterminable ownership record.
                return Err(format!(
                    "could not verify the session file is not held: {error:#}"
                ));
            };
            Err(pa_daemon::hold_refusal::refusal_message(
                &pa_daemon::hold_refusal::HoldIdentity {
                    pid: active.holder_pid,
                    active_session_id: active.active_session_id.clone(),
                },
                Some(session_path),
            ))
        }
    }
}

fn open_session_file(
    path: &std::path::Path,
    session_dir: &std::path::Path,
    fallback_cwd: &std::path::Path,
    explicit_cwd_override: Option<&std::path::Path>,
) -> Result<pa_core::session::manager::SessionManager, String> {
    let session_cwd = stored_session_cwd(path, fallback_cwd, explicit_cwd_override)?;
    Ok(pa_core::session::manager::SessionManager::open(
        &session_cwd,
        session_dir,
        path,
    ))
}

/// The cwd an opened session runs in: the explicit override (main.ts
/// `explicitCwdOverride`), else the stored session cwd, else the
/// fallback. A session stored against a deleted directory must not
/// silently continue somewhere else (main.ts `getMissingSessionCwdIssue`).
fn stored_session_cwd(
    path: &std::path::Path,
    fallback_cwd: &std::path::Path,
    explicit_cwd_override: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, String> {
    let session_cwd = explicit_cwd_override.map_or_else(
        || {
            let header = pa_core::session::manager::read_session_header(path);
            header.filter(|header| !header.cwd.is_empty()).map_or_else(
                || fallback_cwd.to_path_buf(),
                |header| std::path::PathBuf::from(&header.cwd),
            )
        },
        std::path::Path::to_path_buf,
    );
    if !session_cwd.exists() {
        return Err(format!(
            "Stored session working directory does not exist: {}\nSession file: {}\nCurrent working directory: {}",
            session_cwd.display(),
            path.display(),
            fallback_cwd.display()
        ));
    }
    Ok(session_cwd)
}

pub(crate) fn render_selector_error(error: &SessionSelectorError) -> String {
    format!(
        "{}.{}\nOpen prime-agent and press left-arrow to browse sessions.",
        error.message(),
        error.suggestion().unwrap_or_default()
    )
}

/// Model tools for the print runtime: `python_repl` only — the engine adds the kernel-backed tool
/// itself.
fn builtin_tools(_cwd: &std::path::Path) -> Vec<Arc<dyn pa_agent::types::AgentTool>> {
    Vec::new()
}

/// Admit prompts, stream json events when requested, and decide the exit code
/// from the headless terminal result plus the autonomous gate contract.
async fn run_prompts_and_emit(
    engine: &std::sync::Arc<pa_core::session_engine::engine::SessionEngine>,
    model: &Model,
    api_key: Option<String>,
    options: &RunOptions,
    root_node: &std::sync::Arc<InlineSessionNode>,
) -> Result<i32, String> {
    let json_mode = options.app_mode == AppMode::Json;
    // Non-fatal auth notices (a login that could not be saved, a revoked
    // login another one replaces), once per condition: a stderr warning, or
    // an event line in json mode.
    engine.set_auth_notice_sink(std::sync::Arc::new(move |notice| {
        if json_mode {
            println!(
                "{}",
                serde_json::json!({
                    "type": "auth_notice",
                    "provider": notice.provider,
                    "condition": notice.condition,
                    "message": notice.message,
                })
            );
        } else {
            eprintln!("Warning: {}", notice.message);
        }
    }));
    let mut unsubscribe: Option<pa_agent::agent::Subscription> = None;
    if json_mode {
        if let Some(header) = session_header_json(engine).await {
            println!("{header}");
        }
        unsubscribe = Some(
            engine
                .session
                .agent()
                .subscribe(|event, _signal| {
                    Box::pin(async move {
                        if let Some(json) = agent_event_json(&event) {
                            println!("{json}");
                        }
                        Ok(())
                    })
                })
                .await,
        );
    }
    let events: Option<pa_core::session_engine::headless::EventSink> = json_mode
        .then(|| std::sync::Arc::new(|event: &serde_json::Value| println!("{event}")) as _);
    let mut run = PrintRun::start(
        engine,
        model,
        api_key,
        root_node,
        PrintRunConfig {
            events,
            autonomous: options
                .config
                .autonomous
                .as_ref()
                .map(autonomous_runtime_config),
            cwd: options.config.cwd.clone(),
            agent_dir: options.config.agent_dir.clone(),
        },
    )
    .await;
    // The `@file` image attachments ride the initial prompt only; the later CLI messages stay text.
    let prompts = options
        .initial_message
        .iter()
        .map(|prompt| (prompt.clone(), options.initial_images.clone()))
        .chain(
            options
                .messages
                .iter()
                .map(|prompt| (prompt.clone(), Vec::new())),
        )
        .collect();
    let (goal_no_progress_cap, autonomous_stderr) = match run.run(prompts).await? {
        // The rejected prompt wait: print the raw command error to stderr and
        // exit 1 — no later prompts ran, and the disposal drain still runs.
        PrintRunOutcome::CommandFailed(error) => {
            eprintln!("{error}");
            if let Some(subscription) = unsubscribe {
                subscription.unsubscribe().await;
            }
            run.finish().await;
            return Ok(1);
        }
        PrintRunOutcome::Settled {
            goal_no_progress_cap,
            autonomous_stderr,
        } => (goal_no_progress_cap, autonomous_stderr),
    };
    if let Some(subscription) = unsubscribe {
        subscription.unsubscribe().await;
    }
    let state = engine.session.agent().state().await;
    let messages: Vec<pa_types::session::AgentMessage> =
        state.messages.iter().filter_map(json_round_trip).collect();
    let result = pa_core::session_engine::headless::select_headless_terminal_result(&messages);
    // The print-mode exit contract: both output modes derive the exit code from the
    // terminal selection (an errored/aborted assistant, a failed session command, or a
    // failed compaction exits 1; upstream #2976/#2977). Only text mode renders it: the
    // error to stderr, the settled answer to stdout, and the compaction outcomes to
    // stderr; json mode already streamed the events and prints nothing more.
    let mut exit_code = 0;
    if !json_mode && goal_no_progress_cap {
        eprintln!(
            "{}",
            pa_core::session_engine::goal_driver::CONTINUATION_NO_PROGRESS_CAP_REASON
        );
        exit_code = 1;
    }
    if let Some(primary) = result.primary {
        let stderr = primary.stderr_text(&mut exit_code);
        if !json_mode {
            if let Some(stderr) = stderr {
                eprintln!("{stderr}");
            }
            if exit_code == 0
                && let Some(text) = primary.stdout_text() {
                    println!("{text}");
                }
        }
    }
    for outcome in result.compaction_outcomes {
        if !json_mode {
            eprintln!("{}", outcome.content);
        }
        if outcome.outcome == "failed" {
            exit_code = 1;
        }
    }
    // The autonomous contract applies to both output modes.
    if let Some(stderr) = autonomous_stderr {
        eprintln!("{stderr}");
        exit_code = 1;
    }
    // The disposal order: the exit code first, then the teardown drains a
    // compact-trigger auto-refine no later boundary consumed.
    run.finish().await;
    Ok(exit_code)
}

#[cfg(test)]
mod tests {

    /// Runtime cancellation unblocks a pending writer; ownership must remain
    /// held until that blocking writer finishes, even on errors or unwinding.
    #[test]
    fn print_lease_outlives_runtime_writers_on_every_return_path() {
        #[derive(Clone, Copy)]
        enum Outcome {
            Success,
            Error,
            Panic,
        }
        struct Fixture {
            session_path: std::path::PathBuf,
            agent_dir: std::path::PathBuf,
            outcome: Outcome,
        }
        struct UnblockWriter(std::sync::mpsc::Sender<()>);
        impl Drop for UnblockWriter {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        for outcome in [Outcome::Success, Outcome::Error, Outcome::Panic] {
            let home = tempfile::TempDir::new().unwrap();
            let fixture = Fixture {
                session_path: home.path().join("session.jsonl"),
                agent_dir: home.path().join("agent"),
                outcome,
            };
            let held_at_write = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let writer_observation = std::sync::Arc::clone(&held_at_write);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                super::with_print_runtime(&fixture, |fixture, lease| {
                    Box::pin(async move {
                        *lease = Some(
                            pa_daemon::lease::acquire_runtime_session_lease(
                                &fixture.session_path,
                                &fixture.agent_dir,
                            )
                            .unwrap(),
                        );
                        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
                        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                        tokio::spawn(async move {
                            let _unblock = UnblockWriter(cancel_tx);
                            let _ = ready_tx.send(());
                            std::future::pending::<()>().await;
                        });
                        ready_rx.await.unwrap();
                        let session_path = fixture.session_path.clone();
                        let agent_dir = fixture.agent_dir.clone();
                        let (writer_ready_tx, writer_ready_rx) = tokio::sync::oneshot::channel();
                        tokio::task::spawn_blocking(move || {
                            let _ = writer_ready_tx.send(());
                            cancel_rx.recv().unwrap();
                            writer_observation.store(
                                pa_daemon::lease::live_lease_owner(&agent_dir, &session_path)
                                    .is_some(),
                                std::sync::atomic::Ordering::SeqCst,
                            );
                            std::fs::write(&session_path, "writer settled\n").unwrap();
                        });
                        writer_ready_rx.await.unwrap();
                        match fixture.outcome {
                            Outcome::Success => Ok(0),
                            Outcome::Error => Err("failed after lease acquisition".to_string()),
                            Outcome::Panic => panic!("unwind after lease acquisition"),
                        }
                    })
                })
            }));
            match outcome {
                Outcome::Success => assert_eq!(result.unwrap(), Ok(0)),
                Outcome::Error => assert_eq!(
                    result.unwrap(),
                    Err("failed after lease acquisition".to_string())
                ),
                Outcome::Panic => assert!(result.is_err()),
            }
            assert!(held_at_write.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(
                std::fs::read_to_string(&fixture.session_path).unwrap(),
                "writer settled\n"
            );
            assert!(
                pa_daemon::lease::live_lease_owner(&fixture.agent_dir, &fixture.session_path)
                    .is_none()
            );
        }
    }

    /// Serves a settings-declared server through `mcp.config` and resolves settings LIVE: a
    /// rewrite reaches `refresh()`.
    #[tokio::test]
    async fn print_mode_mcp_manager_serves_settings_servers_and_resolves_live() {
        let home = tempfile::TempDir::new().unwrap();
        let cwd = home.path().to_path_buf();
        let agent_dir = home.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::json!({
                "mcpServers": {
                    "fixture-echo": {
                        "type": "stdio",
                        "command": "python3",
                        "args": ["echo.py"]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let built_manager = crate::mcp_login::cli_mcp_manager(&cwd, &agent_dir);
        let manager = std::sync::Arc::new(std::sync::Mutex::new(built_manager));
        let gating_manager = std::sync::Arc::clone(&manager);
        let enabled = tokio::task::spawn_blocking(move || {
            gating_manager
                .lock()
                .unwrap()
                .get_enabled_persistent_generic_servers()
        })
        .await
        .unwrap();
        assert_eq!(enabled, vec!["fixture-echo".to_string()]);
        let mut handlers = pa_core::kernel::shared::HostRequestHandlers::default();
        pa_core::mcp::McpManager::register_host_handlers(&manager, &mut handlers);
        let config = handlers.get("mcp.config").unwrap().clone();
        let result = config(pa_core::kernel::shared::HostRequestPayload {
            data: serde_json::json!({ "server": "fixture-echo" }),
        })
        .await
        .unwrap();
        assert_eq!(result["type"], "stdio");
        assert_eq!(result["command"], "python3");
        assert_eq!(result["args"], serde_json::json!(["echo.py"]));
        std::fs::write(
            agent_dir.join("settings.json"),
            serde_json::json!({
                "mcpServers": {
                    "second-echo": {
                        "type": "stdio",
                        "command": "node",
                        "args": ["echo.mjs"]
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let gating_manager = std::sync::Arc::clone(&manager);
        tokio::task::spawn_blocking(move || {
            let mut manager = gating_manager.lock().unwrap();
            manager.refresh();
            assert_eq!(
                manager.get_enabled_persistent_generic_servers(),
                vec!["second-echo".to_string()]
            );
        })
        .await
        .unwrap();
        let result = config(pa_core::kernel::shared::HostRequestPayload {
            data: serde_json::json!({ "server": "second-echo" }),
        })
        .await
        .unwrap();
        assert_eq!(
            result["command"], "node",
            "the already-registered handler serves the live integrations"
        );
        let withdrawn = config(pa_core::kernel::shared::HostRequestPayload {
            data: serde_json::json!({ "server": "fixture-echo" }),
        })
        .await
        .unwrap();
        assert!(
            withdrawn.as_object().unwrap().is_empty(),
            "the rewrite withdrew the old server from the live integrations"
        );
    }

    /// Local service-catalog sources reach the catalog resolution; dropping the declaration
    /// withdraws the entry on the next resolve.
    #[test]
    fn print_mode_mcp_manager_resolves_declared_catalog_sources() {
        let home = tempfile::TempDir::new().unwrap();
        let agent_dir = home.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let catalog = home.path().join("local-catalog.json");
        std::fs::write(
            &catalog,
            serde_json::json!({
                "version": 1,
                "entries": [{
                    "server": "my-local", "service": "my-local", "label": "My Local",
                    "url": "https://my-local.example/mcp", "aliases": [],
                    "transport": { "type": "http", "url": "https://my-local.example/mcp" },
                    "auth": { "strategy": "oauth", "clientRegistration": "dynamic" },
                    "setup": { "status": "ready" },
                    "verification": { "status": "unverified" },
                    "legacyBuiltin": false,
                    "provenance": [{ "source": "user" }]
                }]
            })
            .to_string(),
        )
        .unwrap();
        let settings = |sources: &[&str]| {
            serde_json::json!({
                "mcpCatalogSources": sources
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .to_string()
        };
        std::fs::write(
            agent_dir.join("settings.json"),
            settings(&[&catalog.display().to_string()]),
        )
        .unwrap();
        let mut manager = crate::mcp_login::cli_mcp_manager(home.path(), &agent_dir);
        let my_local = manager
            .service_descriptors()
            .iter()
            .find(|service| service.service_id == "my-local")
            .expect("declared source entry resolved");
        assert!(my_local.local_source);
        std::fs::write(agent_dir.join("settings.json"), settings(&[])).unwrap();
        manager.refresh();
        assert!(
            !manager
                .service_descriptors()
                .iter()
                .any(|service| service.service_id == "my-local"),
            "the dropped source no longer resolves"
        );
    }
}
