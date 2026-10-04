//! Observability for Prime Agent: the span recorder that writes the shared
//! structured log (`~/.prime/agent/logs/agent.jsonl`, the TS logging
//! contract in `docs/observability.md`), the `prime-agent trace` and
//! `prime-agent health` readers over it, and the optional OTLP/HTTP exporter.
//!
//! Native crates only emit plain `tracing` spans and events; this crate is
//! the subscriber half. `pa-cli` installs it behind its `trace` feature (see
//! `docs/fork-feature-crates.md`); without it nothing here runs.
//!
//! See `README.md` for scope, seams, files, and the field conventions.

mod health;
mod layer;
mod log_file;
mod otlp;
mod record;
mod retained;
mod trace_command;
mod writer;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pa_types::trace_context::{TraceContext, TRACEPARENT_ENV};
use tracing_subscriber::layer::SubscriberExt;

pub use health::run_health_command;
pub use layer::{TraceLayer, JSON_FIELD_SUFFIX};
pub use otlp::{parse_otlp_headers, OtlpConfig, OtlpStats, OTLP_ENDPOINT_ENV, OTLP_HEADERS_ENV};
pub use trace_command::run_trace_command;

/// How long an orderly exit waits for queued log lines and for the OTLP
/// worker, each (TS `DEFAULT_OTLP_SHUTDOWN_TIMEOUT_MS`).
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(1);

/// What a reader command printed and its exit code; the caller writes the
/// lines (stdout, then stderr) and exits with `code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutcome {
    pub code: i32,
    pub stdout: Vec<String>,
    pub stderr: Vec<String>,
}

impl CommandOutcome {
    pub(crate) fn success(stdout: Vec<String>) -> Self {
        CommandOutcome {
            code: 0,
            stdout,
            stderr: Vec::new(),
        }
    }

    pub(crate) fn failure(stderr: Vec<String>) -> Self {
        CommandOutcome {
            code: 1,
            stdout: Vec::new(),
            stderr,
        }
    }
}

/// Recorder settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderConfig {
    /// The structured log (`<agentDir>/logs/agent.jsonl`).
    pub log_path: PathBuf,
    /// The context an external caller handed this process; every root span
    /// becomes its child.
    pub inbound: Option<TraceContext>,
    /// OTLP export, when configured.
    pub otlp: Option<OtlpConfig>,
}

impl RecorderConfig {
    /// The recorder for `log_path` configured from the environment:
    /// `TRACEPARENT` (a malformed value is ignored) and
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` / `OTEL_EXPORTER_OTLP_HEADERS`.
    #[must_use]
    pub fn from_env(log_path: PathBuf, service_version: &str) -> Self {
        RecorderConfig {
            log_path,
            inbound: std::env::var(TRACEPARENT_ENV)
                .ok()
                .as_deref()
                .and_then(TraceContext::parse),
            otlp: OtlpConfig::from_env(service_version),
        }
    }
}

/// Owner of the recorder's background work: drains it on an orderly exit.
#[derive(Clone)]
pub struct RecorderHandle {
    writer: Arc<writer::LogWriter>,
    otlp: Option<Arc<otlp::OtlpExporter>>,
}

impl RecorderHandle {
    /// Wait up to `timeout` for every queued line to reach the log; answers
    /// whether it did.
    #[must_use]
    pub fn flush(&self, timeout: Duration) -> bool {
        self.writer.flush(timeout)
    }

    /// The OTLP exporter's counters, when export is configured.
    #[must_use]
    pub fn otlp_stats(&self) -> Option<OtlpStats> {
        self.otlp.as_ref().map(|otlp| otlp.stats())
    }

    /// Drain for an orderly exit: queued log lines, then the OTLP worker,
    /// each bounded by `timeout`; a hanging disk or collector is abandoned.
    pub fn shutdown(&self, timeout: Duration) {
        let _ = self.writer.flush(timeout);
        if let Some(otlp) = &self.otlp {
            let _ = otlp.shutdown(timeout);
        }
    }
}

/// A recorder layer and its handle, for composing into a subscriber.
#[must_use]
pub fn recorder(config: RecorderConfig) -> (TraceLayer, RecorderHandle) {
    let writer = Arc::new(writer::LogWriter::new(log_file::RotatingLog::new(
        config.log_path,
    )));
    let otlp = config.otlp.map(otlp::OtlpExporter::new);
    let layer = TraceLayer::new(Arc::clone(&writer), config.inbound, otlp.clone());
    (layer, RecorderHandle { writer, otlp })
}

/// Install the recorder as the process-wide subscriber and answer the
/// current trace context for native carriers
/// ([`pa_types::trace_context::current`]). Does no I/O: the log file and
/// the worker threads start with the first record.
///
/// # Errors
///
/// When a global subscriber is already installed.
pub fn install(
    config: RecorderConfig,
) -> Result<RecorderHandle, tracing::subscriber::SetGlobalDefaultError> {
    let (layer, handle) = recorder(config);
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))?;
    pa_types::trace_context::set_current_context_source(layer::current_context);
    Ok(handle)
}

/// Answer [`pa_types::trace_context::current`] from whichever recorder is
/// the current subscriber (installed by [`install`]; tests composing their
/// own subscriber call it once).
pub fn install_context_source() {
    pa_types::trace_context::set_current_context_source(layer::current_context);
}
