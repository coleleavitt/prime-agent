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
}

static INSTALLED: OnceLock<Vec<Arc<dyn SessionFeature>>> = OnceLock::new();

/// Install the process's enabled features. The composition root calls this
/// once, before any session starts; later calls are ignored and return
/// `false`.
pub fn install(features: Vec<Arc<dyn SessionFeature>>) -> bool {
    INSTALLED.set(features).is_ok()
}

/// The installed features, empty when none were installed.
#[must_use]
pub fn installed() -> &'static [Arc<dyn SessionFeature>] {
    INSTALLED.get().map_or(&[], Vec::as_slice)
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

/// Report the end of every agent run of `agent` to `features`; nothing is
/// subscribed when there are no features.
pub(crate) async fn observe_agent_end(
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
            if matches!(event, pa_agent::types::AgentEvent::AgentEnd { .. }) {
                for feature in &features {
                    feature.on_agent_end(&context);
                }
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
        };
        let mut handlers = HostRequestHandlers::default();
        Stub.register_host_handlers(&context, &mut handlers);
        assert!(handlers.get(STUB_REQUEST).is_some());
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
        });
        let (before, after) = tool_call_hooks(&[], &context);
        assert!(before.is_none() && after.is_none());
    }
}
