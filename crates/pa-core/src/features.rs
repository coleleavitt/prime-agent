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

use crate::kernel::shared::HostRequestHandlers;

/// A feature hook's boxed future: hooks run inline on the session's tool
/// path, so implementations bound their own work.
pub type FeatureFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What a feature may know about the session it is joining.
#[derive(Debug, Clone)]
pub struct SessionFeatureContext {
    /// The agent directory (`~/.prime/agent` or its override).
    pub agent_dir: PathBuf,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The session id.
    pub session_id: String,
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

    /// Bound to a name, not inlined: `prompt_guards` reads every literal
    /// `.register("…")` in `src/` as part of the model-facing surface.
    const STUB_REQUEST: &str = "stub.ping";

    struct Stub;

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
            rlm_depth: 0,
        };
        let mut handlers = HostRequestHandlers::default();
        Stub.register_host_handlers(&context, &mut handlers);
        assert!(handlers.get(STUB_REQUEST).is_some());
    }

    /// With no feature installed the agent loop gets no tool hooks at all:
    /// the native product's loop is untouched.
    #[test]
    fn no_features_install_no_loop_hooks() {
        let context = Arc::new(SessionFeatureContext {
            agent_dir: PathBuf::from("/agent"),
            cwd: PathBuf::from("/work"),
            session_id: "s1".to_string(),
            rlm_depth: 0,
        });
        let (before, after) = tool_call_hooks(&[], &context);
        assert!(before.is_none() && after.is_none());
    }
}
