//! Composition of the separately built feature crates (see
//! `docs/fork-feature-crates.md`). Each crate is wired here behind its own
//! Cargo feature; building with `--no-default-features` installs none, which
//! is the native product.

use std::sync::Arc;

use pa_core::features::SessionFeature;

/// The features this build enables, in installation order.
#[must_use]
pub fn enabled_features() -> Vec<Arc<dyn SessionFeature>> {
    #[allow(unused_mut)] // empty until the first session feature crate is wired in
    let mut features: Vec<Arc<dyn SessionFeature>> = Vec::new();
    features
}

/// What the enabled features keep running for the life of the process; the
/// binary hands it back through [`InstalledFeatures::finish`] before it
/// exits.
pub struct InstalledFeatures {
    /// The trace recorder (`feature = "trace"`): `agent.jsonl` writes and
    /// the optional OTLP export, both on background workers.
    #[cfg(feature = "trace")]
    trace: Option<pa_trace::RecorderHandle>,
}

impl InstalledFeatures {
    /// Drain the features' background work for an orderly exit, each part
    /// bounded (a hanging disk or collector is abandoned).
    pub fn finish(&self) {
        #[cfg(feature = "trace")]
        if let Some(trace) = &self.trace {
            trace.shutdown(pa_trace::SHUTDOWN_DRAIN);
        }
    }
}

/// Install the enabled features: the session seam, and (feature `trace`)
/// the trace recorder as the process subscriber. Called once by the binary
/// before any session or worker starts. Does no I/O: the recorder opens its
/// log and starts its workers with the first record.
#[must_use]
pub fn install_enabled_features() -> InstalledFeatures {
    pa_core::features::install(enabled_features());
    InstalledFeatures {
        #[cfg(feature = "trace")]
        trace: pa_trace::install(pa_trace::RecorderConfig::from_env(
            trace_log_path(),
            crate::config::VERSION,
        ))
        .ok(),
    }
}

/// The shared structured log (`<agentDir>/logs/agent.jsonl`).
#[cfg(feature = "trace")]
fn trace_log_path() -> std::path::PathBuf {
    crate::config::get_agent_dir()
        .join("logs")
        .join("agent.jsonl")
}

/// The log readers `pa-trace` provides.
#[cfg(feature = "trace")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TraceReader {
    /// `prime-agent trace <traceId|traceparent>`.
    Trace,
    /// `prime-agent health`.
    Health,
}

/// Run a reader over the agent log, print its output (stdout, then stderr),
/// record the `observability command used` adoption event, and answer the
/// exit code.
#[cfg(feature = "trace")]
pub(crate) fn run_trace_reader(reader: TraceReader, args: &[String]) -> i32 {
    let started = std::time::Instant::now();
    let log_path = trace_log_path();
    let (command, outcome) = match reader {
        TraceReader::Trace => ("trace", pa_trace::run_trace_command(args, &log_path)),
        TraceReader::Health => (
            "health",
            pa_trace::run_health_command(args, &log_path, crate::util_time::now_ms() as i64),
        ),
    };
    let pa_trace::CommandOutcome {
        code,
        stdout,
        stderr,
    } = outcome;
    for line in stdout {
        println!("{line}");
    }
    for line in stderr {
        eprintln!("{line}");
    }
    // `observability command used`: primitives only (command, exit class,
    // duration), delivered before the command exits like `update completed`;
    // a reader is never on a paint path.
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let agent_dir = crate::config::get_agent_dir();
    let settings = pa_core::settings::SettingsManager::create(&cwd, &agent_dir);
    if crate::mode::telemetry_disabled(&settings) {
        return code;
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return code;
    };
    runtime.block_on(async {
        let client = pa_core::session_engine::telemetry::build_client(&settings, &agent_dir);
        let mut properties = pa_telemetry::base_properties("cli");
        properties.set("command", serde_json::Value::from(command));
        let outcome = match code {
            0 => "ok",
            2 => "unhealthy",
            _ => "error",
        };
        properties.set("outcome", serde_json::Value::from(outcome));
        properties.set("duration_ms", serde_json::Value::from(duration_ms));
        client.track("observability command used", properties);
        let _ = client.shutdown().await;
    });
    code
}
