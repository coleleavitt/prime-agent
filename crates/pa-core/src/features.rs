//! The session-feature seam: optional, separately built capabilities plug
//! into every session through this registry, so native crates never name
//! them (see `docs/fork-feature-crates.md`).
//!
//! The composition root installs the enabled features once, at process
//! start; [`crate::session_engine::engine::create_session`] asks each
//! installed feature to register its kernel host-request handlers for the
//! session it is building and, when any feature is installed, routes the
//! session's tool-call and run-end lifecycle through them. With nothing
//! installed (the native product, and every native test) the seam is a
//! no-op: no hook is installed on the agent loop at all.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use pa_agent::agent_loop::{AfterToolCallFn, BeforeToolCallFn};
use pa_agent::types::{AfterToolCallResult, AgentMessage, Message, ToolResultContent};

use pa_telemetry::{base_properties, lookup, Properties, TelemetryClient};

use crate::kernel::shared::HostRequestHandlers;
use crate::refinement::gate::RefinementGate;
use crate::refinement::prompt_hook::{HarnessPromptHook, HarnessPromptHooks};
use crate::session_engine::telemetry::TelemetryWiring;

/// A feature hook's boxed future: hooks run inline on the session's tool
/// path, so implementations bound their own work.
pub type FeatureFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What a feature may know about the session it is joining.
#[derive(Clone)]
pub struct SessionFeatureContext {
    /// The agent directory (`~/.prime/agent` or its override).
    pub agent_dir: PathBuf,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The session id.
    pub session_id: String,
    /// Import names of the Python skills this session's kernel binds.
    pub python_skill_import_names: Vec<String>,
    /// The model the session was created with: the default a feature's own
    /// model selectors fall back to (like `model.info`, a creation-time
    /// fact).
    pub model: pa_agent::types::Model,
    /// The session's telemetry; `None` for sessions without telemetry
    /// (opted out, tests, one-shot paths).
    pub telemetry: Option<FeatureTelemetry>,
    /// The session's depth in the RLM recursion tree: 0 for a top-level
    /// session, N for a child spawned at depth N.
    pub rlm_depth: u32,
    /// The session's artifact directory (where its local harness state,
    /// kernel snapshot and other per-session files live); `None` for a
    /// session that persists nothing.
    pub session_artifact_dir: Option<PathBuf>,
}

/// A tool call the loop is about to execute (arguments validated).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallObservation {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
}

/// A tool call's executed result, before the loop finalizes it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultObservation {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub is_error: bool,
    /// The result's content blocks as the tool returned them, before any
    /// feature appended text.
    pub content: Vec<ToolResultContent>,
    /// The result's structured details (`AgentToolResult::details`); `Null`
    /// when it reported none.
    pub details: serde_json::Value,
    /// Host-side facts the tool reported (`AgentToolResult::host_facts`);
    /// `Null` when it reported none.
    pub host_facts: serde_json::Value,
    /// Results of the same tool already in the session's context, this call
    /// excluded (a resumed session counts its restored history).
    pub earlier_results_of_tool: usize,
}

/// A feature's handle onto its session's telemetry client: catalogued
/// events only, stamped with the session's base properties, gated live on
/// the session's opt-out switch. Delivery is the client's background
/// worker's (fire-and-forget).
#[derive(Clone)]
pub struct FeatureTelemetry {
    track: Arc<TrackFn>,
}

/// A tracking function: event name and the feature's own properties.
type TrackFn = dyn Fn(&str, &Properties) + Send + Sync;

impl std::fmt::Debug for FeatureTelemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FeatureTelemetry")
    }
}

impl FeatureTelemetry {
    /// Wrap a raw tracking function (a feature test's recorder). Unlike
    /// [`Self::from_wiring`] it applies no catalog check or switch.
    pub fn new(track: impl Fn(&str, Properties) + Send + Sync + 'static) -> Self {
        Self {
            track: Arc::new(move |name, properties: &Properties| track(name, properties.clone())),
        }
    }

    /// The handle over one session's telemetry wiring (the engine builds
    /// it per session; a feature's tests build it over a mock-sink client).
    #[must_use]
    pub fn from_wiring(wiring: &TelemetryWiring) -> Self {
        let client: TelemetryClient = wiring.client.clone();
        let execution_mode = wiring.execution_mode.clone();
        let enabled = wiring
            .telemetry_enabled
            .as_ref()
            .map(|switch| Arc::clone(&switch.enabled));
        Self {
            track: Arc::new(move |name, properties| {
                if lookup(name).is_none() || enabled.as_ref().is_some_and(|enabled| !enabled()) {
                    return;
                }
                let mut tracked = base_properties(
                    execution_mode
                        .as_deref()
                        .unwrap_or(crate::session_engine::telemetry::EXECUTION_MODE_UNKNOWN),
                );
                tracked.merge(properties);
                client.track(name, tracked);
            }),
        }
    }

    /// Queue one event. Tracks nothing when the name is not in the
    /// `pa-telemetry` catalog or telemetry is switched off right now; the
    /// catalog's typed rules normalize the properties on delivery.
    pub fn track(&self, name: &str, properties: &Properties) {
        (self.track)(name, properties);
    }
}

/// What a feature slash command produced.
pub struct FeatureCommandOutcome {
    /// The command's durable result row.
    pub text: String,
    /// Background work whose settlement is reported as a later result row
    /// (`Ok` text as a success row, `Err` as `Command failed: <message>`).
    pub completion: Option<FeatureFuture<Result<String, String>>>,
}

impl std::fmt::Debug for FeatureCommandOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureCommandOutcome")
            .field("text", &self.text)
            .field("completion", &self.completion.is_some())
            .finish()
    }
}

/// A feature's live status for one session: what a session-event surface
/// (the daemon's `feature_status` event and roster summary, the agents view)
/// shows. Replaces the feature's previous status for that session.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureStatus {
    /// The publishing feature (`SessionFeature::name`).
    pub feature: String,
    /// A one-line human summary the agents view shows; `None` clears it.
    pub line: Option<String>,
    /// The feature's own structured status (opaque to native code).
    pub status: serde_json::Value,
}

/// Where a session's feature statuses go (the embedding's event surface).
pub type FeatureStatusSink = Arc<dyn Fn(FeatureStatus) + Send + Sync>;

type StatusSinks = std::sync::Mutex<
    std::collections::HashMap<String, std::sync::Weak<dyn Fn(FeatureStatus) + Send + Sync>>,
>;

fn status_sinks() -> &'static StatusSinks {
    static SINKS: OnceLock<StatusSinks> = OnceLock::new();
    SINKS.get_or_init(StatusSinks::default)
}

/// Route session `session_id`'s feature statuses to `sink` while the caller
/// keeps it alive (the registry holds it weakly); replaces an earlier sink.
pub fn register_feature_status_sink(session_id: &str, sink: &FeatureStatusSink) {
    let mut sinks = status_sinks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    sinks.retain(|_, sink| sink.strong_count() > 0);
    sinks.insert(session_id.to_string(), Arc::downgrade(sink));
}

/// Publish a feature's status for a session; `false` when the session has
/// no live sink (an embedding without an event surface, or a closed
/// session). Never blocks on the surface beyond the sink's own call.
pub fn publish_feature_status(session_id: &str, status: FeatureStatus) -> bool {
    let sink = status_sinks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(session_id)
        .and_then(std::sync::Weak::upgrade);
    match sink {
        Some(sink) => {
            sink(status);
            true
        }
        None => false,
    }
}

/// One optional capability. Implementations live in their own crates and
/// are installed by the composition root; every method has a no-op default
/// so a feature implements only the seams it uses. Hooks receive the
/// session's context on every call: one feature instance serves every
/// session in the process, so per-session state is keyed by
/// [`SessionFeatureContext::session_id`].
pub trait SessionFeature: Send + Sync {
    /// Stable feature name (`trace`, `recall`, ...), used for diagnostics.
    fn name(&self) -> &'static str;

    /// Register this feature's kernel host-request handlers for one session.
    /// Handlers registered here override native ones of the same type, so
    /// a feature must only claim request types it owns.
    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        let _ = (context, handlers);
    }

    /// Observe a tool call before it executes. The loop awaits the future
    /// before running the tool, so it must stay short and bounded; it
    /// cannot block or alter the call.
    fn before_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        call: &ToolCallObservation,
    ) -> FeatureFuture<()> {
        let _ = (context, call);
        Box::pin(async {})
    }

    /// Observe a tool call's result before the loop finalizes it; text
    /// returned here is appended to the result the model sees, as its own
    /// text block. Awaited on the tool path, so it must stay bounded.
    fn after_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        result: &ToolResultObservation,
    ) -> FeatureFuture<Option<String>> {
        let _ = (context, result);
        Box::pin(async { None })
    }

    /// The session was created: `history` is the conversation it resumed
    /// with (empty for a new session). Called once, before the session's
    /// first run, on the session-creation path, so it must not block.
    fn on_session_start(&self, context: &Arc<SessionFeatureContext>, history: &[AgentMessage]) {
        let _ = (context, history);
    }

    /// A message was finalized into the session's context (a prompt, an
    /// assistant reply, a tool result, a steering message), in order.
    /// Called from the run's event stream: it must not block, so durable
    /// work is handed to the feature's own background worker.
    fn on_message_end(&self, context: &Arc<SessionFeatureContext>, message: &AgentMessage) {
        let _ = (context, message);
    }

    /// The session's agent run ended. Called from the run's event stream:
    /// it must not block, so durable work is handed to the feature's own
    /// background worker.
    fn on_agent_end(&self, context: &Arc<SessionFeatureContext>) {
        let _ = context;
    }

    /// The process is about to exit: finish or abandon background work by
    /// `deadline`. Called once, outside any async runtime.
    fn flush(&self, deadline: Instant) {
        let _ = deadline;
    }

    /// The gate this feature judges the session's refinements with (see
    /// [`crate::refinement::gate`]); `None`, the default, leaves them
    /// ungated. Called once, on the session-creation path; the first
    /// installed feature that returns a gate is the session's gate.
    fn refinement_gate(
        &self,
        context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn RefinementGate>> {
        let _ = context;
        None
    }

    /// Session slash commands this feature contributes (`/name args`). The
    /// composition root's [`install`] registers them in the shared
    /// `pa_types::slash_commands` registry, so every surface (TUI
    /// autocomplete and dispatch, daemon and print-mode admission) treats
    /// them as session commands; a name a builtin owns is dropped. Read once,
    /// at install.
    fn slash_commands(&self) -> Vec<pa_types::slash_commands::BuiltinSlashCommand> {
        Vec::new()
    }

    /// Execute one of this feature's slash commands in a session; `None`
    /// when `name` is not this feature's. The outcome's text is the
    /// command's durable result row; its `completion`, when given, is
    /// awaited in the background and its text (or error) appended as a
    /// second durable result row when it settles (a command that starts
    /// background work reports how the work ended). An `Err` is the
    /// command's failure (`Command failed: <message>`).
    fn execute_slash_command(
        &self,
        context: &Arc<SessionFeatureContext>,
        name: &str,
        args: &str,
    ) -> Option<FeatureFuture<Result<FeatureCommandOutcome, String>>> {
        let _ = (context, name, args);
        None
    }

    /// Built-in skills this feature contributes: directory names under the
    /// bundled skills directory's hidden feature directory
    /// (`skills/.features/<name>/`, see
    /// [`crate::packages::FEATURE_SKILLS_DIR`]). They load as built-in skills
    /// (Python ones are installed into the kernel) only while the feature is
    /// installed; native scans never see them. Empty by default.
    fn bundled_skills(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// The hook this feature adjusts the session's harness digest with (see
    /// [`crate::refinement::prompt_hook`]); `None`, the default, leaves the
    /// digest native. Called once, on the session-creation path; every
    /// installed feature's hook applies, in installation order.
    fn harness_prompt_hook(
        &self,
        context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn HarnessPromptHook>> {
        let _ = context;
        None
    }
}

static INSTALLED: OnceLock<Vec<Arc<dyn SessionFeature>>> = OnceLock::new();

/// Install the process's enabled features. The composition root calls this
/// once, before any session starts; later calls are ignored and return
/// `false`.
pub fn install(features: Vec<Arc<dyn SessionFeature>>) -> bool {
    let commands: Vec<_> = features
        .iter()
        .flat_map(|feature| feature.slash_commands())
        .collect();
    let installed = INSTALLED.set(features).is_ok();
    if installed && !commands.is_empty() {
        pa_types::slash_commands::register_feature_slash_commands(commands);
    }
    installed
}

/// The installed features, empty when none were installed.
#[must_use]
pub fn installed() -> &'static [Arc<dyn SessionFeature>] {
    INSTALLED.get().map_or(&[], Vec::as_slice)
}

/// The built-in skill directory names every installed feature contributes.
#[must_use]
pub fn installed_bundled_skills() -> Vec<String> {
    installed()
        .iter()
        .flat_map(|feature| feature.bundled_skills())
        .map(str::to_string)
        .collect()
}

/// Run the installed feature that owns slash command `name`; `None` when
/// none does.
pub(crate) fn execute_feature_slash_command(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    name: &str,
    args: &str,
) -> Option<FeatureFuture<Result<FeatureCommandOutcome, String>>> {
    features
        .iter()
        .find_map(|feature| feature.execute_slash_command(context, name, args))
}

/// Let every installed feature register its handlers for one session.
pub fn register_session_host_handlers(
    context: &SessionFeatureContext,
    handlers: &mut HostRequestHandlers,
) {
    for feature in installed() {
        feature.register_host_handlers(context, handlers);
    }
}

/// Give every installed feature until `timeout` from now to finish its
/// background work. The composition root calls this once before the
/// process exits; with nothing installed it returns at once.
pub fn flush_installed(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    for feature in installed() {
        feature.flush(deadline);
    }
}

/// The agent-loop tool hooks that route one session's tool calls through
/// `features`; `None` for both when there are no features, so the native
/// loop runs without hooks.
pub(crate) fn tool_call_hooks(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
) -> (Option<BeforeToolCallFn>, Option<AfterToolCallFn>) {
    if features.is_empty() {
        return (None, None);
    }
    let before: BeforeToolCallFn = {
        let features = features.to_vec();
        let context = Arc::clone(context);
        Arc::new(move |call, _signal| {
            let observation = ToolCallObservation {
                tool_call_id: call.tool_call.id.clone(),
                tool_name: call.tool_call.name.clone(),
                args: call.args,
            };
            let pending: Vec<_> = features
                .iter()
                .map(|feature| feature.before_tool_call(&context, &observation))
                .collect();
            Box::pin(async move {
                futures::future::join_all(pending).await;
                Ok(None)
            })
        })
    };
    let after: AfterToolCallFn = {
        let features = features.to_vec();
        let context = Arc::clone(context);
        Arc::new(move |call, _signal| {
            let earlier_results_of_tool = call
                .context
                .messages
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        AgentMessage::Standard(Message::ToolResult(result))
                            if result.tool_name == call.tool_call.name
                                && result.tool_call_id != call.tool_call.id
                    )
                })
                .count();
            let observation = ToolResultObservation {
                tool_call_id: call.tool_call.id.clone(),
                tool_name: call.tool_call.name.clone(),
                args: call.args,
                is_error: call.is_error,
                content: call.result.content.clone(),
                details: call.result.details.clone(),
                host_facts: call.result.host_facts.clone(),
                earlier_results_of_tool,
            };
            let pending: Vec<_> = features
                .iter()
                .map(|feature| feature.after_tool_call(&context, &observation))
                .collect();
            let mut blocks = call.result.content;
            Box::pin(async move {
                let appended: Vec<String> = futures::future::join_all(pending)
                    .await
                    .into_iter()
                    .flatten()
                    .collect();
                if appended.is_empty() {
                    return Ok(None);
                }
                blocks.extend(appended.into_iter().map(ToolResultContent::text));
                Ok(Some(AfterToolCallResult {
                    content: Some(blocks),
                    ..AfterToolCallResult::default()
                }))
            })
        })
    };
    (Some(before), Some(after))
}

/// The first refinement gate `features` offer the session; `None` when no
/// feature judges refinements.
pub(crate) fn session_refinement_gate(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
) -> Option<Arc<dyn RefinementGate>> {
    features
        .iter()
        .find_map(|feature| feature.refinement_gate(context))
}

/// The harness digest hooks `features` offer the session, in order; empty
/// when no feature adjusts the digest.
pub(crate) fn session_harness_prompt_hooks(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
) -> HarnessPromptHooks {
    HarnessPromptHooks(
        features
            .iter()
            .filter_map(|feature| feature.harness_prompt_hook(context))
            .collect(),
    )
}

/// Tell `features` a session was created with `history`; nothing happens
/// when there are no features.
pub(crate) fn observe_session_start(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    history: &[AgentMessage],
) {
    for feature in features {
        feature.on_session_start(context, history);
    }
}

/// Report every finalized message and the end of every agent run of
/// `agent` to `features`; nothing is subscribed when there are no features.
pub(crate) async fn observe_agent_events(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    agent: &pa_agent::agent::Agent,
) {
    if features.is_empty() {
        return;
    }
    let features = features.to_vec();
    let context = Arc::clone(context);
    // The subscription lives as long as the agent: dropping the guard
    // keeps the listener.
    let _subscription = agent
        .subscribe(move |event, _signal| {
            match &event {
                pa_agent::types::AgentEvent::MessageEnd { message } => {
                    for feature in &features {
                        feature.on_message_end(&context, message);
                    }
                }
                pa_agent::types::AgentEvent::AgentEnd { .. } => {
                    for feature in &features {
                        feature.on_agent_end(&context);
                    }
                }
                _ => {}
            }
            Box::pin(async { Ok(()) })
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Named through a const: `prompt_guards` reads every request-type
    /// literal registered in `src/` as model-facing surface.
    const STUB_REQUEST: &str = "stub.ping";

    struct Stub;

    fn stub_model() -> pa_agent::types::Model {
        serde_json::from_value(serde_json::json!({
            "id": "m1", "name": "M1", "api": "test", "provider": "p1",
            "baseUrl": "http://localhost", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap()
    }

    impl SessionFeature for Stub {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn register_host_handlers(
            &self,
            context: &SessionFeatureContext,
            handlers: &mut HostRequestHandlers,
        ) {
            let cwd = context.cwd.display().to_string();
            // No dot: prompt_guards scans the sources for dotted request
            // literals and requires the core prompt to document each one.
            handlers.register(
                STUB_REQUEST,
                crate::kernel::shared::host_handler(move |_| {
                    let cwd = cwd.clone();
                    async move { Ok(serde_json::json!({ "cwd": cwd })) }
                }),
            );
        }
    }

    /// A feature's default methods are no-ops, and a registered feature's
    /// handlers reach the registry the engine hands it.
    #[test]
    fn a_feature_registers_handlers_through_the_seam() {
        let context = SessionFeatureContext {
            agent_dir: PathBuf::from("/agent"),
            cwd: PathBuf::from("/work"),
            session_id: "s1".to_string(),
            python_skill_import_names: Vec::new(),
            model: stub_model(),
            telemetry: None,
            rlm_depth: 0,
            session_artifact_dir: None,
        };
        let mut handlers = HostRequestHandlers::default();
        Stub.register_host_handlers(&context, &mut handlers);
        assert!(handlers.get(STUB_REQUEST).is_some());
    }

    /// A harness-render hook a stub feature offers reaches the session's
    /// digest hooks, after the features that offer none.
    #[test]
    fn a_feature_offers_a_harness_prompt_hook_through_the_seam() {
        struct Ranked;
        impl HarnessPromptHook for Ranked {
            fn adjust(
                &self,
                _state: &crate::refinement::HarnessState,
            ) -> crate::refinement::prompt_hook::HarnessPromptAdjustment {
                crate::refinement::prompt_hook::HarnessPromptAdjustment {
                    entry_rank: [("a".to_string(), -1)].into_iter().collect(),
                    ..Default::default()
                }
            }
        }
        struct Hooked;
        impl SessionFeature for Hooked {
            fn name(&self) -> &'static str {
                "hooked"
            }
            fn harness_prompt_hook(
                &self,
                _context: &Arc<SessionFeatureContext>,
            ) -> Option<Arc<dyn HarnessPromptHook>> {
                Some(Arc::new(Ranked))
            }
        }
        let context = Arc::new(SessionFeatureContext {
            agent_dir: PathBuf::from("/agent"),
            cwd: PathBuf::from("/work"),
            session_id: "s1".to_string(),
            python_skill_import_names: Vec::new(),
            model: stub_model(),
            telemetry: None,
            rlm_depth: 0,
            session_artifact_dir: None,
        });
        assert!(session_harness_prompt_hooks(&[], &context).0.is_empty());
        let features: Vec<Arc<dyn SessionFeature>> = vec![Arc::new(Stub), Arc::new(Hooked)];
        let hooks = session_harness_prompt_hooks(&features, &context);
        assert_eq!(hooks.0.len(), 1);
        let adjustment = hooks
            .adjust(&crate::refinement::empty_harness_state())
            .expect("a ranking adjustment");
        assert_eq!(adjustment.rank("a"), -1);
    }

    /// A feature's telemetry handle tracks catalogued events through the
    /// session's client with the base properties stamped, and refuses
    /// uncatalogued names and events while the opt-out switch is off.
    #[tokio::test]
    async fn feature_telemetry_tracks_catalogued_events_behind_the_switch() {
        let mock = Arc::new(pa_telemetry::MockSink::new());
        let mut config = pa_telemetry::TelemetryClientConfig::new("install-1");
        config.batch_size = 1;
        config.sinks = vec![Arc::clone(&mock) as Arc<dyn pa_telemetry::TelemetrySink>];
        let client = TelemetryClient::spawn(config).unwrap();
        let on = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let probe = Arc::clone(&on);
        let telemetry = FeatureTelemetry::from_wiring(&TelemetryWiring {
            client: client.clone(),
            execution_mode: Some("print".to_string()),
            now: None,
            telemetry_enabled: Some(crate::session_engine::telemetry::RecordingSwitch::test(
                Arc::new(move || probe.load(std::sync::atomic::Ordering::SeqCst)),
            )),
        });
        let mut properties = Properties::new();
        properties.set("platform", serde_json::json!("linux"));

        telemetry.track("computer_use_session_started", &properties);
        telemetry.track("not a catalogued event", &properties);
        on.store(false, std::sync::atomic::Ordering::SeqCst);
        telemetry.track("computer_use_session_started", &properties);
        client.flush().await.unwrap();

        let events = mock.events();
        let names: Vec<&str> = events.iter().map(|event| event.name.as_str()).collect();
        assert_eq!(names, ["computer_use_session_started"]);
        let mut expected = base_properties("print");
        expected.merge(&properties);
        let delivered: Vec<(&String, &serde_json::Value)> = events[0]
            .properties
            .iter()
            .filter(|(key, _)| expected.get(key).is_some())
            .collect();
        assert_eq!(delivered, expected.iter().collect::<Vec<_>>());
    }

    /// With no feature installed the agent loop gets no tool hooks at all:
    /// the native product's loop is untouched.
    #[test]
    fn no_features_install_no_loop_hooks() {
        let context = Arc::new(SessionFeatureContext {
            agent_dir: PathBuf::from("/agent"),
            cwd: PathBuf::from("/work"),
            session_id: "s1".to_string(),
            python_skill_import_names: Vec::new(),
            model: stub_model(),
            telemetry: None,
            rlm_depth: 0,
            session_artifact_dir: None,
        });
        let (before, after) = tool_call_hooks(&[], &context);
        assert!(before.is_none() && after.is_none());
    }

    struct GateStub;

    impl crate::refinement::gate::RefinementGate for GateStub {
        fn evaluate(
            &self,
            _request: crate::refinement::gate::RefinementGateRequest,
        ) -> FeatureFuture<
            anyhow::Result<Option<Box<dyn crate::refinement::gate::RefinementGateVerdict>>>,
        > {
            Box::pin(async { Ok(None) })
        }
    }

    struct Gating;

    impl SessionFeature for Gating {
        fn name(&self) -> &'static str {
            "gating"
        }

        fn refinement_gate(
            &self,
            _context: &Arc<SessionFeatureContext>,
        ) -> Option<Arc<dyn RefinementGate>> {
            Some(Arc::new(GateStub))
        }
    }

    /// No feature offers a gate in the native product; with features
    /// installed, the first one that offers a gate is the session's.
    #[test]
    fn the_first_offered_refinement_gate_is_the_sessions() {
        let context = Arc::new(SessionFeatureContext {
            agent_dir: PathBuf::from("/agent"),
            cwd: PathBuf::from("/work"),
            session_id: "s1".to_string(),
            python_skill_import_names: Vec::new(),
            model: stub_model(),
            telemetry: None,
            rlm_depth: 0,
            session_artifact_dir: None,
        });
        assert!(session_refinement_gate(&[], &context).is_none());
        let features: Vec<Arc<dyn SessionFeature>> = vec![Arc::new(Stub), Arc::new(Gating)];
        assert!(session_refinement_gate(&features, &context).is_some());
    }

    struct CommandStub;

    impl SessionFeature for CommandStub {
        fn name(&self) -> &'static str {
            "command-stub"
        }

        fn slash_commands(&self) -> Vec<pa_types::slash_commands::BuiltinSlashCommand> {
            vec![pa_types::slash_commands::BuiltinSlashCommand {
                name: "stub-command",
                description: "A stub command",
                execution: pa_types::slash_commands::SlashCommandExecution::Session,
                argument_hint: None,
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
            if name != "stub-command" {
                return None;
            }
            let text = format!("{} ran {args}", context.session_id);
            let fail = args == "fail";
            Some(Box::pin(async move {
                if fail {
                    return Err("stub refused".to_string());
                }
                Ok(FeatureCommandOutcome {
                    text,
                    completion: Some(Box::pin(async { Ok("stub finished".to_string()) })),
                })
            }))
        }
    }

    /// The owning feature runs a slash command; nobody owns an unknown one.
    #[tokio::test]
    async fn a_feature_runs_its_own_slash_command() {
        let context = Arc::new(SessionFeatureContext {
            agent_dir: PathBuf::from("/agent"),
            cwd: PathBuf::from("/work"),
            session_id: "s1".to_string(),
            python_skill_import_names: Vec::new(),
            model: stub_model(),
            telemetry: None,
            rlm_depth: 0,
            session_artifact_dir: None,
        });
        let features: Vec<Arc<dyn SessionFeature>> = vec![Arc::new(Stub), Arc::new(CommandStub)];
        assert!(execute_feature_slash_command(&features, &context, "nope", "").is_none());
        let outcome = execute_feature_slash_command(&features, &context, "stub-command", "x")
            .expect("owned")
            .await
            .expect("ran");
        assert_eq!(outcome.text, "s1 ran x");
        assert_eq!(
            outcome.completion.expect("background work").await,
            Ok("stub finished".to_string())
        );
        let refused = execute_feature_slash_command(&features, &context, "stub-command", "fail")
            .expect("owned")
            .await;
        assert_eq!(refused.err(), Some("stub refused".to_string()));
        assert_eq!(CommandStub.slash_commands()[0].name, "stub-command");
        assert!(Stub.slash_commands().is_empty());
    }

    /// A session's statuses reach the sink registered for it while the
    /// sink lives; none reach a dropped or unregistered one.
    #[test]
    fn feature_statuses_reach_the_sessions_live_sink() {
        let seen: Arc<std::sync::Mutex<Vec<FeatureStatus>>> = Arc::default();
        let recorder = Arc::clone(&seen);
        let sink: FeatureStatusSink = Arc::new(move |status| recorder.lock().unwrap().push(status));
        register_feature_status_sink("status-session-1", &sink);
        let status = FeatureStatus {
            feature: "stub".to_string(),
            line: Some("stub: running".to_string()),
            status: serde_json::json!({ "phase": "running" }),
        };
        assert!(publish_feature_status("status-session-1", status.clone()));
        assert!(!publish_feature_status("status-session-2", status.clone()));
        drop(sink);
        assert!(!publish_feature_status("status-session-1", status.clone()));
        assert_eq!(*seen.lock().unwrap(), vec![status]);
    }
}
