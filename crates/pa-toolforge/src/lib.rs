//! # pa-toolforge
//!
//! The host half of `rlm.toolforge.publish`: the kernel runtime sends a
//! `toolforge.publish` host request carrying a skill name, its source, a
//! description and an exit test; this crate stages the package, runs the
//! double-run gate (the exit test must fail against an unimplemented stub and
//! pass against the real package), promotes an accepted package into
//! `<agentDir>/skills/<name>`, editable-installs it into the kernel venv and
//! records every attempt in `<agentDir>/toolforge/ledger.json`. The runtime
//! then binds the module in the publishing cell; later sessions discover it as
//! an ordinary Python skill.
//!
//! The crate plugs into sessions through
//! [`pa_core::features::SessionFeature`]; see `README.md`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pa_core::features::{FeatureTelemetry, SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::{host_handler, HostRequestHandlers};
use serde_json::Value;

mod gate;
mod ledger;
mod name;
mod package;
mod publish;

pub use gate::DEFAULT_GATE_TIMEOUT;
pub use ledger::{
    ledger_path, load_ledger, published_packages, GatePhase, GateRun, Ledger, LedgerRecord,
    PublishStatus, PublishedPackage,
};
pub use name::{validate_name, RESERVED_IMPORT_NAMES};
pub use publish::{
    kernel_venv_installer, publish, InstallFuture, PackageInstaller, PromoteStep, PublishOptions,
    PublishRequest, PublishResult, RejectionStage,
};

/// The kernel host request this crate serves.
pub const PUBLISH_REQUEST: &str = "toolforge.publish";
/// The adoption event one publish attempt reports.
pub const PUBLISH_EVENT: &str = "toolforge publish";

/// Per-build overrides of where a publish writes and what it runs with; the
/// defaults derive from the session's agent directory.
#[derive(Clone, Default)]
pub struct ToolforgeOverrides {
    pub skills_dir: Option<PathBuf>,
    pub ledger_path: Option<PathBuf>,
    pub staging_dir: Option<PathBuf>,
    /// Interpreter for the gate runs (default: the kernel python on disk).
    pub python: Option<PathBuf>,
    /// Promote + install (default: the kernel-venv installer).
    pub installer: Option<PackageInstaller>,
    pub timeout: Option<Duration>,
}

/// The toolforge session feature.
#[derive(Clone, Default)]
pub struct ToolforgeFeature {
    overrides: ToolforgeOverrides,
}

impl ToolforgeFeature {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A feature whose publishes write and run where `overrides` say.
    #[must_use]
    pub fn with_overrides(overrides: ToolforgeOverrides) -> Self {
        Self { overrides }
    }

    fn options_for(&self, context: &SessionFeatureContext) -> PublishOptions {
        let defaults = PublishOptions::for_agent_dir(&context.agent_dir);
        let overrides = self.overrides.clone();
        PublishOptions {
            skills_dir: overrides.skills_dir.unwrap_or(defaults.skills_dir),
            ledger_path: overrides.ledger_path.unwrap_or(defaults.ledger_path),
            staging_dir: overrides.staging_dir.unwrap_or(defaults.staging_dir),
            python: overrides.python,
            loaded_import_names: context.python_skill_import_names.clone(),
            installer: overrides.installer.unwrap_or(defaults.installer),
            timeout: overrides.timeout.unwrap_or(defaults.timeout),
            session_id: Some(context.session_id.clone()).filter(|id| !id.is_empty()),
        }
    }
}

impl SessionFeature for ToolforgeFeature {
    fn name(&self) -> &'static str {
        "toolforge"
    }

    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        register_publish_handler(
            handlers,
            self.options_for(context),
            context.telemetry.clone(),
        );
    }
}

fn string_field(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The adoption event: outcome categories and counts only, never a name,
/// source or path.
fn track_publish(telemetry: &FeatureTelemetry, result: &PublishResult, duration: Duration) {
    let mut properties = pa_telemetry::Properties::new();
    properties.set("status", result.status.as_str().into());
    properties.set(
        "rejection",
        result
            .rejection
            .map_or(Value::Null, |stage| stage.as_str().into()),
    );
    properties.set("installed", result.installed.into());
    properties.set("gate_run_count", result.gate.len().into());
    properties.set("version", result.version.into());
    properties.set(
        "duration_ms",
        u64::try_from(duration.as_millis())
            .unwrap_or(u64::MAX)
            .into(),
    );
    telemetry.track(PUBLISH_EVENT, properties);
}

/// Register `toolforge.publish`. Publishes from one session run one at a
/// time: two at once would race over the same staging root and the same
/// bootstrap lock for no benefit.
pub fn register_publish_handler(
    handlers: &mut HostRequestHandlers,
    options: PublishOptions,
    telemetry: Option<FeatureTelemetry>,
) {
    let options = Arc::new(options);
    let queue = Arc::new(tokio::sync::Mutex::new(()));
    handlers.register(
        PUBLISH_REQUEST,
        host_handler(move |payload| {
            let options = Arc::clone(&options);
            let queue = Arc::clone(&queue);
            let telemetry = telemetry.clone();
            async move {
                let request = PublishRequest {
                    name: string_field(&payload.data, "name"),
                    source: string_field(&payload.data, "source"),
                    doc: string_field(&payload.data, "doc"),
                    exit_test: string_field(&payload.data, "exit_test"),
                };
                let _turn = queue.lock().await;
                let started = std::time::Instant::now();
                let result = publish(&request, &options).await;
                if let Some(telemetry) = &telemetry {
                    track_publish(telemetry, &result, started.elapsed());
                }
                Ok(result.to_response())
            }
        }),
    );
}
