//! The bundled computer-use skill's host side: the `computer_use.*` kernel
//! host requests, served by `pa-computer-use`.
//!
//! The kernel's `computer_use` package is a thin client of these requests;
//! every observation, input, capture, allowlist and permission decision
//! runs in the host. One [`ComputerUse`] per session holds the bound apps
//! (the Python module's per-kernel state). Its adoption events go through
//! the same validated path as the kernel's `telemetry.emit` bridge, so an
//! opted-out session (or one whose live switch reads off) records nothing.

use std::path::Path;
use std::sync::Arc;

use pa_computer_use::telemetry::{NoTelemetry, TelemetryEvent, TelemetrySink};
use pa_computer_use::{ComputerUse, HostConfig};
use pa_telemetry::TelemetryClient;
use serde_json::{json, Value};

use super::host_requests::handle_telemetry_emit_host_request;
use super::telemetry::{RecordingSwitch, TelemetryWiring, EXECUTION_MODE_UNKNOWN};
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// Forwards the crate's events through the kernel bridge's validation.
struct BridgeTelemetry {
    client: TelemetryClient,
    execution_mode: String,
    enabled: Option<RecordingSwitch>,
}

impl TelemetrySink for BridgeTelemetry {
    fn track(&self, event: TelemetryEvent) {
        if self
            .enabled
            .as_ref()
            .is_some_and(|switch| !(switch.enabled)())
        {
            return;
        }
        let payload = json!({"name": event.name(), "properties": event.properties()});
        let _ = handle_telemetry_emit_host_request(&payload, &self.client, &self.execution_mode);
    }
}

fn sink(telemetry: Option<&TelemetryWiring>) -> Arc<dyn TelemetrySink> {
    match telemetry {
        Some(wiring) => Arc::new(BridgeTelemetry {
            client: wiring.client.clone(),
            execution_mode: wiring
                .execution_mode
                .clone()
                .unwrap_or_else(|| EXECUTION_MODE_UNKNOWN.to_string()),
            enabled: wiring.telemetry_enabled.clone(),
        }),
        None => Arc::new(NoTelemetry),
    }
}

/// Register the `computer_use.*` requests for one session. The backend is
/// detected per request (a niri session can start after the kernel) and
/// built on first use; nothing touches the desktop until the skill calls.
pub(crate) fn register_host_handlers(
    handlers: &mut HostRequestHandlers,
    agent_dir: &Path,
    telemetry: Option<&TelemetryWiring>,
) {
    let host = Arc::new(ComputerUse::new(HostConfig {
        agent_dir: agent_dir.to_path_buf(),
        telemetry: sink(telemetry),
    }));
    // Spelled out (equal to `REQUEST_TYPES`, asserted below) so the prompt
    // guards see the registered types.
    for request_type in [
        "computer_use.get_state",
        "computer_use.list_apps",
        "computer_use.permissions_status",
        "computer_use.get_app",
        "computer_use.app",
    ] {
        let host = Arc::clone(&host);
        handlers.register(
            request_type,
            host_handler(move |payload| {
                let host = Arc::clone(&host);
                async move { Ok::<Value, anyhow::Error>(host.handle(request_type, payload.data).await) }
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_computer_use::REQUEST_TYPES;
    use pa_telemetry::MockSink;

    // Only the registration is exercised here: invoking a request detects
    // the live desktop's backend, and tests never touch the user's session.
    // The routing and the wire shapes are tested in `pa-computer-use`.
    #[test]
    fn every_request_type_is_registered() {
        let agent_dir = tempfile::tempdir().unwrap();
        let mut handlers = HostRequestHandlers::new();
        register_host_handlers(&mut handlers, agent_dir.path(), None);
        for kind in REQUEST_TYPES {
            assert!(handlers.get(kind).is_some(), "{kind}");
        }
        assert_eq!(handlers.len(), REQUEST_TYPES.len());
    }

    fn client(mock: &Arc<MockSink>) -> TelemetryClient {
        let mut config = pa_telemetry::TelemetryClientConfig::new("install-1");
        config.batch_size = 1;
        config.flush_interval = std::time::Duration::from_mins(10);
        config.sinks = vec![mock.clone() as Arc<dyn pa_telemetry::TelemetrySink>];
        TelemetryClient::spawn(config).expect("spawn client")
    }

    fn bridge(mock: &Arc<MockSink>, on: bool) -> BridgeTelemetry {
        BridgeTelemetry {
            client: client(mock),
            execution_mode: "interactive".to_string(),
            enabled: Some(RecordingSwitch::test(Arc::new(move || on))),
        }
    }

    #[tokio::test]
    async fn actions_ride_the_bridge_and_an_opted_out_switch_records_nothing() {
        let mock = Arc::new(MockSink::new());
        let sink = bridge(&mock, true);
        sink.track(TelemetryEvent::Action {
            action: "click",
            outcome: pa_computer_use::telemetry::Outcome::Error(
                pa_computer_use::error::ErrorCode::AppNotAllowed,
            ),
            duration_ms: 12,
        });
        sink.client.flush().await.unwrap();
        let events = mock.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "computer_use_action");
        assert_eq!(
            events[0].properties.get("error_code"),
            Some(&json!("APP_NOT_ALLOWED"))
        );
        assert_eq!(
            events[0].properties.get("execution_mode"),
            Some(&json!("interactive"))
        );

        let mock = Arc::new(MockSink::new());
        let sink = bridge(&mock, false);
        sink.track(TelemetryEvent::SessionStarted { platform: "linux" });
        sink.client.flush().await.unwrap();
        assert!(mock.events().is_empty());
    }
}
