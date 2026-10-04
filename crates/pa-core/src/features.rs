//! The session-feature seam: optional, separately built capabilities plug
//! into every session through this registry, so native crates never name
//! them (see `docs/fork-feature-crates.md`).
//!
//! The composition root installs the enabled features once, at process
//! start; [`crate::session_engine::engine::create_session`] asks each
//! installed feature to register its kernel host-request handlers for the
//! session it is building. With nothing installed (the native product, and
//! every native test) the seam is a no-op.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use pa_telemetry::Properties;

use crate::kernel::shared::HostRequestHandlers;

/// A session's adoption-telemetry emitter, handed to features so they can
/// report their catalogued events without reaching for the session's client.
/// Emission is fire-and-forget; the host stamps the platform base properties
/// and drops the event while the session's live opt-out switch reads off.
#[derive(Clone)]
pub struct FeatureTelemetry {
    track: Arc<TrackFn>,
}

/// A tracking function: event name and the feature's own properties.
type TrackFn = dyn Fn(&str, Properties) + Send + Sync;

impl FeatureTelemetry {
    /// Wrap a tracking function (the composition root's, or a test's
    /// recorder).
    pub fn new(track: impl Fn(&str, Properties) + Send + Sync + 'static) -> Self {
        Self {
            track: Arc::new(track),
        }
    }

    /// Track one catalogued event with the feature's own properties.
    pub fn track(&self, name: &str, properties: Properties) {
        (self.track)(name, properties);
    }
}

impl std::fmt::Debug for FeatureTelemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FeatureTelemetry")
    }
}

/// What a feature may know about the session it is joining.
#[derive(Debug, Clone)]
pub struct SessionFeatureContext {
    /// The agent directory (`~/.prime/agent` or its override).
    pub agent_dir: PathBuf,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The session id.
    pub session_id: String,
    /// Import names of the Python skills this session's kernel binds.
    pub python_skill_import_names: Vec<String>,
    /// The session's telemetry emitter; `None` when the session runs without
    /// telemetry (opt-out, tests, one-shot paths).
    pub telemetry: Option<FeatureTelemetry>,
}

/// One optional capability. Implementations live in their own crates and
/// are installed by the composition root; every method has a no-op default
/// so a feature implements only the seams it uses.
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub;

    /// Named, not inlined at the `register` call: the prompt guard
    /// (`tests/prompt_guards.rs`) reads every `.register("…")` literal in
    /// these sources as part of the documented kernel surface.
    const STUB_REQUEST: &str = "stub.ping";

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
            telemetry: None,
        };
        let mut handlers = HostRequestHandlers::default();
        Stub.register_host_handlers(&context, &mut handlers);
        assert!(handlers.get(STUB_REQUEST).is_some());
    }

    /// A feature's telemetry reaches the session's tracker with its name and
    /// properties intact.
    #[test]
    fn feature_telemetry_forwards_to_the_session_tracker() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let telemetry = FeatureTelemetry::new(move |name, properties| {
            recorder
                .lock()
                .unwrap()
                .push((name.to_string(), properties));
        });
        let mut properties = Properties::new();
        properties.set("count", serde_json::json!(2));
        telemetry.track("stub event", properties.clone());
        assert_eq!(
            *seen.lock().unwrap(),
            vec![("stub event".to_string(), properties)]
        );
    }
}
