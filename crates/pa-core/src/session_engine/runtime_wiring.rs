//! Wires the session runtime (goal + rlm-heartbeat host bridge) and the Python kernel
//! into the session engine build (the product-path equivalent of the TS `AgentSession`
//! host-request controllers): the kernel provisioner receives the host-handler registry,
//! and the agent loop gains the `ipython` tool backed by that kernel.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::runtime::QueuedGoalContextPurge;
use crate::cron::store::AgentCronJobStore;
use crate::kernel::bootstrap::KernelPythonSkill;
use crate::kernel::provisioner::{
    IpythonKernelProvisioner as KernelProvisioner, IpythonKernelProvisionerOptions,
};
use crate::kernel::shared::HostRequestHandlers;
use crate::session::manager::SessionManager;
use crate::skills::{get_python_skill_runtime_info, Skill};
use crate::tools::ipython::{
    IpythonKernelProvisioner, IpythonToolOptions, KernelAttachment, KernelErrorInfo,
    KernelExecError, KernelExecuteOptions, KernelExecutor,
};

use super::host_requests::SessionBinding;
use super::rlm_host::{register_rlm_host_handlers, RlmHostBridge, RlmSubagentHost};
use super::runtime::SessionRuntime;

/// RLM inputs the session composition supplies: a shared model registry and the daemon
/// child-session host; defaults derive from `agent_dir` or the no-children behavior.
#[derive(Default)]
pub struct RlmWiring {
    /// Registry `rlm.find_models` searches. Defaults to the `agent_dir` catalog.
    pub model_registry: Option<Arc<crate::models::registry::ModelRegistry>>,
    /// Child-session machinery backing `rlm.spawn`/`rlm.create_session` and
    /// the roster/collect/delete surface.
    pub subagent_host: Option<Arc<dyn RlmSubagentHost>>,
}

/// The embedding's cron wiring for the kernel's `rlm_heartbeat.*` host
/// requests: the shared store plus the durable session identity the
/// kernel-created jobs bind to.
#[derive(Clone)]
pub struct KernelCronWiring {
    /// The daemon worker's scheduled-jobs store.
    pub store: std::sync::Arc<crate::cron::store::AgentCronJobStore>,
    /// `None` until the embedding knows it (the engine falls back to the
    /// in-memory manager's identity).
    pub binding: Option<KernelCronBinding>,
    /// The post-mutation seam for kernel `rlm_heartbeat.*` requests;
    /// `None` leaves mutations unannounced (the embedded default).
    pub mutation_hook: Option<super::host_requests::RlmHeartbeatMutationHook>,
}

/// The live/durable session identity for kernel-created rlm heartbeats.
#[derive(Clone, Debug)]
pub struct KernelCronBinding {
    /// The live active session id the daemon routes commands by.
    pub active_session_id: String,
    /// The durable session id the store partitions by.
    pub session_id: String,
    /// The durable session file the rebind pass moves jobs through.
    pub session_file: String,
    /// The session working directory.
    pub cwd: String,
}

// Opaque like the store it carries: the store handle has no meaningful
// debug form, and config structs embedding the wiring derive `Debug`.
impl std::fmt::Debug for KernelCronWiring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelCronWiring")
            .field("binding", &self.binding)
            // `..` documents the deliberately-opaque store + hook.
            .finish_non_exhaustive()
    }
}

/// Session-scoped runtime wiring: the shared session manager handle,
/// the kernel host-handler registry, and the runtime itself.
pub struct SessionKernelWiring {
    pub session: Arc<tokio::sync::Mutex<SessionManager>>,
    pub handlers: HostRequestHandlers,
    pub runtime: Arc<SessionRuntime>,
    /// The RLM bridge: progress-note state the daemon roster reads.
    pub rlm: Arc<RlmHostBridge>,
    /// The child-usage attribution producer the daemon's children
    /// registry drives after the engine is built.
    pub rlm_usage: Arc<super::rlm_usage::RlmChildUsageAttributions>,
    /// The session's factory executor (kernel `factory.*` requests and the
    /// `/factory` lane).
    pub factory: Arc<crate::factory::executor::FactoryExecutor>,
}

/// Build the session runtime and register the `goal.*`, `rlm_heartbeat.*`, and `rlm.*`
/// host handlers; `goal_complete_purge` is the embedding's queued-goal-context purge.
#[must_use]
pub fn wire_session_runtime(
    session: SessionManager,
    agent_dir: &std::path::Path,
    rlm: RlmWiring,
    goal_complete_purge: Option<QueuedGoalContextPurge>,
    cron_store: Option<KernelCronWiring>,
) -> SessionKernelWiring {
    // The embedding's durable session identity overrides the in-memory manager's
    // when supplied: kernel-created rlm heartbeats must bind the live session id the
    // supervisor routes by, plus the durable id + file (the partition the store writes
    // and the rebind pass moves onto live sessions).
    let fallback_binding = || {
        (
            session.get_session_id().to_string(),
            SessionBinding {
                session_id: session.get_session_id().to_string(),
                session_file: session
                    .get_session_file()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                cwd: session.get_cwd().display().to_string(),
            },
        )
    };
    let (active_session_id, binding) = match cron_store
        .as_ref()
        .and_then(|wiring| wiring.binding.as_ref())
    {
        Some(binding) => (
            binding.active_session_id.clone(),
            SessionBinding {
                session_id: binding.session_id.clone(),
                session_file: binding.session_file.clone(),
                cwd: binding.cwd.clone(),
            },
        ),
        None => fallback_binding(),
    };
    // An embedding-owned store replaces the engine-private one, so
    // kernel `rlm_heartbeat.*` writes reach the daemon catalog; the
    // private file store remains the embedded/standalone default.
    let mutation_hook = cron_store
        .as_ref()
        .and_then(|wiring| wiring.mutation_hook.clone());
    let cron_store = cron_store.map_or_else(
        || Arc::new(AgentCronJobStore::new(agent_dir.join("cron-jobs.json"))),
        |wiring| wiring.store,
    );
    // Factory run records live beside the session's other artifacts, so a
    // restarted host recovers its runs: the persisted session's artifact
    // dir, else the one the embedding's durable session file implies (the
    // daemon worker's engine session is in memory; the worker owns the
    // file). A session with neither keeps its runs in memory.
    let factory_store = session
        .get_session_artifact_dir()
        .or_else(|| {
            Some(binding.session_file.as_str())
                .filter(|file| !file.is_empty())
                .and_then(|file| {
                    super::harness_digest::session_artifact_dir_for_log(std::path::Path::new(file))
                })
        })
        .map(|dir| dir.join(crate::factory::executor::store::FACTORY_RUNS_DIR));
    let mut runtime = SessionRuntime::new(&session, cron_store, active_session_id, binding);
    if let Some(purge) = goal_complete_purge {
        runtime.set_goal_complete_purge(purge);
    }
    if let Some(hook) = mutation_hook {
        runtime.set_cron_mutation_hook(hook);
    }
    let runtime = Arc::new(runtime);
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let mut handlers = HostRequestHandlers::default();
    runtime.register_host_handlers(session.clone(), &mut handlers);
    let model_registry = rlm.model_registry.unwrap_or_else(|| {
        let auth = crate::auth::AuthStorage::create(agent_dir);
        let mut registry =
            crate::models::registry::ModelRegistry::create(auth, agent_dir.join("models.json"));
        // Adopt the on-disk private authorization before freezing the Arc:
        // `rlm.find_models` and child-spawn resolution search this registry, and a
        // fresh registry otherwise gates every private `internal/*` model out (only
        // the async refresh populates the authorized set).
        registry.load_private_authorization_from_cache();
        Arc::new(registry)
    });
    let rlm_usage = Arc::new(super::rlm_usage::RlmChildUsageAttributions::new(
        session.clone(),
    ));
    // The daemon supplies the child-session host; an embedding without
    // one keeps the no-children behavior, whose self-rename appends the
    // session's own `session_info` name row.
    let subagent_host = rlm
        .subagent_host
        .unwrap_or_else(|| Arc::new(super::rlm_host::NoRlmChildren::new(session.clone())));
    let rlm_bridge = Arc::new(RlmHostBridge::new(
        model_registry,
        subagent_host,
        rlm_usage.clone(),
    ));
    register_rlm_host_handlers(&mut handlers, &rlm_bridge);
    // The kernel's `rlm.harness` store calls.
    crate::refinement::store::register_host_handlers(&mut handlers);
    // The factory executor runs host-side over the same child host the
    // kernel's `rlm.spawn` uses: a kernel restart never touches a run.
    let factory_children = Arc::new(crate::factory::executor::ports::SessionChildren::new(
        Arc::clone(&rlm_bridge),
    ));
    let factory = Arc::new(crate::factory::executor::FactoryExecutor::new(
        crate::factory::executor::FactoryExecutorConfig {
            children: factory_children,
            notices: Arc::new(crate::factory::executor::ports::NoNoticeLane),
            clock: Arc::new(crate::factory::executor::ports::SystemClock),
            store_dir: factory_store,
        },
    ));
    crate::factory::host::register_factory_spec_handler(&mut handlers);
    crate::factory::host::register_factory_executor_handlers(&mut handlers, &factory, agent_dir);
    SessionKernelWiring {
        session,
        handlers,
        runtime,
        rlm: rlm_bridge,
        rlm_usage,
        factory,
    }
}

/// Kernel-side Python skill modules, pre-imported at bootstrap.
#[must_use]
pub fn kernel_python_skills(skills: &[Skill]) -> Vec<KernelPythonSkill> {
    get_python_skill_runtime_info(skills)
        .into_iter()
        .map(|info| KernelPythonSkill {
            name: info.name,
            import_name: info.import_name,
            package_path: info.package_path,
            pyproject_path: info.pyproject_path,
        })
        .collect()
}

/// The variables a session's kernel gets on top of what it inherits: its
/// agent dir (an embedding host's ambient one must not leak in, #109).
pub(crate) fn kernel_env_overrides(agent_dir: &std::path::Path) -> HashMap<String, String> {
    HashMap::from([(
        "PRIME_AGENT_CODING_AGENT_DIR".to_string(),
        agent_dir.to_string_lossy().to_string(),
    )])
}

/// Build the kernel provisioner for a session: host handlers for the goal/heartbeat bridge
/// plus the pre-imported skills. The session's agent dir is propagated into the kernel env
/// (`PRIME_AGENT_CODING_AGENT_DIR`): an embedding host whose ambient env differs must not
/// leak its own paths into the kernel (#109). `cwd` is the SESSION's working directory.
#[allow(clippy::too_many_arguments)] // one wiring funnel, same style as AgentSession::from_session_arc
#[must_use]
pub fn kernel_provisioner(
    session_id: String,
    handlers: HostRequestHandlers,
    python_skills: Vec<KernelPythonSkill>,
    cwd: std::path::PathBuf,
    agent_dir: &std::path::Path,
    snapshot_dir: Option<std::path::PathBuf>,
    on_restore: Option<crate::kernel::provisioner::RestoreCallback>,
    on_background_work_settled: Option<crate::kernel::shared::BackgroundWorkSettledCallback>,
    on_unavailable_skills: Option<crate::kernel::provisioner::UnavailableSkillsCallback>,
    on_bootstrap_result: Option<crate::kernel::provisioner::KernelBootstrapResultHandler>,
    environment: crate::kernel::shared::KernelEnvironment,
    plan_mode: crate::kernel::plan_guard::PlanModeSwitch,
) -> Arc<KernelProvisioner> {
    let mut env = kernel_env_overrides(agent_dir);
    env.extend(kernel_harness_env(agent_dir, snapshot_dir.as_deref()));
    Arc::new(KernelProvisioner::new(
        cwd,
        IpythonKernelProvisionerOptions {
            python: None,
            env,
            command_prefix: None,
            shell_path: None,
            session_id: Some(session_id),
            host_handlers: handlers,
            python_skills,
            // Only persistent sessions (which have an artifact dir) get
            // a revivable snapshot (TS `snapshotDir`).
            snapshot_dir,
            ready_gate: None,
            on_restore,
            on_background_work_settled,
            on_unavailable_skills,
            on_bootstrap_result,
            environment,
            plan_mode: Some(plan_mode),
        },
    ))
}

/// The harness stores the kernel's `rlm.harness` resolves (TS
/// `agent-session.ts` exports the same variables): the global store, the
/// session's local store when the session persists, and the binary a plain
/// Python process the kernel starts reaches the store through.
fn kernel_harness_env(
    agent_dir: &std::path::Path,
    session_artifact_dir: Option<&std::path::Path>,
) -> Vec<(String, String)> {
    let mut env = vec![(
        "RLM_GLOBAL_HARNESS_STATE_DIR".to_string(),
        crate::refinement::get_global_harness_state_dir(agent_dir)
            .to_string_lossy()
            .to_string(),
    )];
    if let Some(local) = crate::refinement::get_local_harness_state_dir(session_artifact_dir) {
        env.push((
            "RLM_HARNESS_STATE_DIR".to_string(),
            local.to_string_lossy().to_string(),
        ));
    }
    // A store the host process itself was pointed at (an eval harness
    // seeding `RLM_HARNESS_STATE_DIR`) reaches the kernel unchanged.
    env.retain(|(name, _)| {
        std::env::var_os(name).is_none_or(|value| value.to_string_lossy().trim().is_empty())
    });
    // Only the product binary serves the one-shot; a test harness or an
    // embedding binary does not.
    if let Ok(executable) = std::env::current_exe() {
        if executable
            .file_stem()
            .is_some_and(|stem| stem == "prime-agent")
        {
            env.push((
                "PRIME_AGENT_EXECUTABLE".to_string(),
                executable.to_string_lossy().to_string(),
            ));
        }
    }
    env
}

impl IpythonKernelProvisioner for KernelProvisioner {
    fn ensure(
        &self,
        on_progress: Option<crate::tools::ipython::BootstrapProgressHandler>,
        signal: Option<crate::tools::tool_definition::AbortSignal>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn KernelExecutor>>> + Send>> {
        let this = self.clone();
        Box::pin(async move {
            // The tool contract uses the raw cancellation token; the kernel
            // wraps it in its own abort signal.
            let signal = signal.map(crate::kernel::cancellation::AbortSignal::from_token);
            let manager = this.ensure(on_progress, signal).await?;
            Ok(Box::new(KernelManagerExecutor { manager }) as Box<dyn KernelExecutor>)
        })
    }

    fn kill(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let this = self.clone();
        Box::pin(async move {
            this.kill();
        })
    }

    fn take_unreported_exit(&self) -> Option<crate::kernel::shared::KernelUnexpectedExit> {
        KernelProvisioner::take_unreported_exit(self)
    }
}

/// Adapts the kernel manager to the ipython tool's executor contract.
struct KernelManagerExecutor {
    manager: crate::kernel::manager::ReplKernelManager,
}

impl KernelExecutor for KernelManagerExecutor {
    fn execute(
        &self,
        code: &str,
        options: KernelExecuteOptions<'_>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<crate::tools::ipython::ExecuteResult, KernelExecError>>
                + Send,
        >,
    > {
        let manager = self.manager.clone();
        let code = code.to_string();
        let signal = options.signal;
        let timeout_ms = options.timeout_ms;
        let on_late_sent_agent_message = options.on_late_sent_agent_message;
        Box::pin(async move {
            let result = manager
                .execute_bounded(
                    &code,
                    crate::kernel::shared::ExecuteOptions {
                        signal: signal.map(crate::kernel::cancellation::AbortSignal::from_token),
                        on_late_sent_agent_message,
                        timeout_excludes_host_requests: true,
                        ..Default::default()
                    },
                    timeout_ms,
                )
                .await
                .map_err(classify_execute_error)?;
            Ok(convert_execute_result(result))
        })
    }
}

/// Type the manager's execute failure for the tool: a dead kernel, a kernel
/// still busy with an interrupted cell, or anything else.
fn classify_execute_error(error: anyhow::Error) -> KernelExecError {
    let error = match error.downcast::<crate::kernel::shared::KernelExitedError>() {
        Ok(exited) => return KernelExecError::KernelExited(exited),
        Err(error) => error,
    };
    if error.is::<crate::kernel::shared::KernelBusyAfterInterruptError>() {
        return KernelExecError::BusyAfterInterrupt(
            crate::tools::ipython::KernelBusyAfterInterruptError::default(),
        );
    }
    KernelExecError::Other(error)
}

fn convert_status(
    status: crate::kernel::shared::ExecuteStatus,
) -> crate::tools::ipython::ExecuteStatus {
    match status {
        crate::kernel::shared::ExecuteStatus::Ok => crate::tools::ipython::ExecuteStatus::Ok,
        crate::kernel::shared::ExecuteStatus::Error => crate::tools::ipython::ExecuteStatus::Error,
        crate::kernel::shared::ExecuteStatus::Aborted => {
            crate::tools::ipython::ExecuteStatus::Aborted
        }
    }
}

fn convert_execute_result(
    result: crate::kernel::shared::ExecuteResult,
) -> crate::tools::ipython::ExecuteResult {
    crate::tools::ipython::ExecuteResult {
        status: convert_status(result.status),
        stdout: result.stdout,
        stderr: result.stderr,
        result: result.result,
        duration_ms: Some(result.duration_ms),
        background_output: result.background_output,
        error: result.error.map(|error| KernelErrorInfo {
            ename: error.ename,
            evalue: error.evalue,
            traceback: error.traceback,
        }),
        attachments: result
            .attachments
            .unwrap_or_default()
            .into_iter()
            .map(|attachment| KernelAttachment {
                mime_type: attachment.mime_type,
                data: attachment.data,
            })
            .collect(),
        sent_agent_messages: result.sent_agent_messages.unwrap_or_default(),
        bash_commands: result.bash_commands,
        executed_bash_commands: result.executed_bash_commands,
        timed_out: result.timed_out,
        kernel_unresponsive: result.kernel_unresponsive,
    }
}

/// Build the ipython tool options for a wired kernel provisioner.
#[must_use]
pub fn ipython_tool_options(
    provisioner: Arc<KernelProvisioner>,
    on_late_sent_agent_message: Option<crate::tools::ipython::LateSentAgentMessageHandler>,
) -> IpythonToolOptions {
    IpythonToolOptions {
        provisioner,
        ui: None,
        on_late_sent_agent_message,
        cell_timeout_ms: crate::tools::ipython::resolve_cell_timeout_ms(
            std::env::var(crate::tools::ipython::IPYTHON_CELL_TIMEOUT_ENV)
                .ok()
                .as_deref(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_managers_busy_after_interrupt_failure_reaches_the_tool_typed() {
        // Untyped, the tool's busy-kernel handling (wait/kill choice, or
        // the headless replacement) never ran: every later call failed.
        let busy = classify_execute_error(anyhow::Error::new(
            crate::kernel::shared::KernelBusyAfterInterruptError,
        ));
        assert!(busy.is_busy_after_interrupt(), "{}", busy.message());
        let other = classify_execute_error(anyhow::anyhow!("Kernel has been shut down"));
        assert_eq!(
            (other.is_busy_after_interrupt(), other.message()),
            (false, "Kernel has been shut down".to_string())
        );
    }
}
