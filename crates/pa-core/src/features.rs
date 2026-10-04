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

use pa_telemetry::{base_properties, lookup, Properties, TelemetryClient};

use crate::kernel::shared::HostRequestHandlers;
use crate::session_engine::telemetry::TelemetryWiring;

/// What a feature may know about the session it is joining.
#[derive(Clone)]
pub struct SessionFeatureContext {
    /// The agent directory (`~/.prime/agent` or its override).
    pub agent_dir: PathBuf,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// The session id.
    pub session_id: String,
    /// The model the session was created with: the default a feature's own
    /// model selectors fall back to (like `model.info`, a creation-time
    /// fact).
    pub model: pa_agent::types::Model,
    /// The session's telemetry; `None` for sessions without telemetry
    /// (opted out, tests, one-shot paths).
    pub telemetry: Option<FeatureTelemetry>,
}

/// A feature's handle onto its session's telemetry client: catalogued
/// events only, stamped with the session's base properties, gated live on
/// the session's opt-out switch. Delivery is the client's background
/// worker's (fire-and-forget).
#[derive(Clone)]
pub struct FeatureTelemetry {
    client: TelemetryClient,
    execution_mode: Option<String>,
    enabled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl FeatureTelemetry {
    /// The handle over one session's telemetry wiring (the engine builds
    /// it per session; a feature's tests build it over a mock-sink client).
    #[must_use]
    pub fn from_wiring(wiring: &TelemetryWiring) -> Self {
        Self {
            client: wiring.client.clone(),
            execution_mode: wiring.execution_mode.clone(),
            enabled: wiring
                .telemetry_enabled
                .as_ref()
                .map(|switch| Arc::clone(&switch.enabled)),
        }
    }

    /// Queue one event. Tracks nothing when the name is not in the
    /// `pa-telemetry` catalog or telemetry is switched off right now; the
    /// catalog's typed rules normalize the properties on delivery.
    pub fn track(&self, name: &str, properties: &Properties) {
        if lookup(name).is_none() || self.enabled.as_ref().is_some_and(|enabled| !enabled()) {
            return;
        }
        let mut tracked = base_properties(
            self.execution_mode
                .as_deref()
                .unwrap_or(crate::session_engine::telemetry::EXECUTION_MODE_UNKNOWN),
        );
        tracked.merge(properties);
        self.client.track(name, tracked);
    }
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
                "stub.ping",
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
            model: serde_json::from_value(serde_json::json!({
                "id": "m1", "name": "M1", "api": "test", "provider": "p1",
                "baseUrl": "http://localhost", "reasoning": false,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                "contextWindow": 1000, "maxTokens": 100
            }))
            .unwrap(),
            telemetry: None,
        };
        let mut handlers = HostRequestHandlers::default();
        Stub.register_host_handlers(&context, &mut handlers);
        assert!(handlers.get("stub.ping").is_some());
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
}
