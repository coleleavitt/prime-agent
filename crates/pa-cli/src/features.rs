//! Composition of the separately built feature crates (see
//! `docs/fork-feature-crates.md`). Each crate is wired here behind its own
//! Cargo feature; building with `--no-default-features` installs none, which
//! is the native product.

use std::sync::Arc;
use std::time::Duration;

use pa_core::features::SessionFeature;

/// The features this build enables, in installation order.
#[must_use]
// One cfg-gated push per feature crate (attributes on `vec!` elements are
// not stable); with every feature compiled out nothing is pushed.
#[allow(unused_mut, clippy::vec_init_then_push)]
pub fn enabled_features() -> Vec<Arc<dyn SessionFeature>> {
    let mut features: Vec<Arc<dyn SessionFeature>> = vec![
        #[cfg(feature = "recall")]
        Arc::new(pa_recall::WorkspaceRecall::default()),
        #[cfg(feature = "toolforge")]
        Arc::new(pa_toolforge::ToolforgeFeature::new()),
        #[cfg(feature = "workflow")]
        Arc::new(pa_workflow::WorkflowFeature),
        #[cfg(feature = "learning")]
        Arc::new(pa_learning::LearningFeature),
        // In-session Dream-RSI: the `dream.*` host requests, `/dream`, the
        // bundled `dream` skill (top-level sessions with harness state).
        #[cfg(feature = "dream")]
        Arc::new(pa_dream::session::DreamFeature::new()),
    ];
    #[cfg(feature = "ledger")]
    features.extend(ledger_features());
    features
}

/// The failure ledger, and (feature `ravo`, which implies `ledger`) RAVO
/// observing it: the ledger reports to RAVO's observer, and RAVO reads the
/// ledger through its handle. RAVO is installed first, so at exit its
/// replay self-checks finish before the ledger's flush writes them.
#[cfg(feature = "ledger")]
fn ledger_features() -> Vec<Arc<dyn SessionFeature>> {
    #[cfg(feature = "ravo")]
    {
        let ravo = pa_ravo::RavoFeature::new(pa_ravo::RavoOptions {
            enabled: None,
            runner: Arc::new(pa_ravo::PythonReplayRunner::default()),
            replay_sys_path: Vec::new(),
        });
        let ledger = pa_ledger::FailureLedgerFeature::with_observers(
            pa_ledger::LedgerOptions::default(),
            vec![ravo.ledger_observer()],
        );
        ravo.attach_ledger(ledger.handle());
        // The trajectory's internalized fingerprints mute their recurrence
        // reminders (feature `learning`, which implies `ravo`).
        #[cfg(feature = "learning")]
        ravo.attach_recurrence_filter(pa_learning::LearningFeature.recurrence_filter());
        vec![Arc::new(ravo), Arc::new(ledger)]
    }
    #[cfg(not(feature = "ravo"))]
    {
        vec![Arc::new(pa_ledger::FailureLedgerFeature::default())]
    }
}

/// What the enabled features keep running for the life of the process; the
/// binary hands it back through [`InstalledFeatures::finish`] before it
/// exits.
pub struct InstalledFeatures {
    /// The trace recorder (`feature = "trace"`): `agent.jsonl` writes and
    /// the optional OTLP export, both on background workers.
    #[cfg(feature = "trace")]
    trace: Option<pa_trace::RecorderHandle>,
    /// The saved-session catalog index (`feature = "session-index"`):
    /// its writes run on a writer thread.
    #[cfg(feature = "session-index")]
    session_index: pa_session_index::SessionIndex,
}

impl InstalledFeatures {
    /// Drain the features' background work for an orderly exit, each part
    /// bounded (a hanging disk or collector is abandoned).
    pub fn finish(&self) {
        #[cfg(feature = "trace")]
        if let Some(trace) = &self.trace {
            trace.shutdown(pa_trace::SHUTDOWN_DRAIN);
        }
        // A write cut off at exit leaves the previous index in place (the
        // rename is atomic); the bound only spares the next process a fold.
        #[cfg(feature = "session-index")]
        let _idle = self
            .session_index
            .flush(std::time::Instant::now() + FEATURE_FLUSH_TIMEOUT);
    }
}

/// Install the enabled features: the session seam, the TUI seams
/// ([`install_tui_features`]), (feature `trace`) the trace recorder as the
/// process subscriber, and (feature `session-index`) the saved-session
/// catalog cache. Called once by the binary before any
/// session or worker starts. Does no I/O: the recorder opens its log and
/// starts its workers with the first record; the index reads a session
/// directory on its first listing.
#[must_use]
pub fn install_enabled_features() -> InstalledFeatures {
    pa_core::features::install(enabled_features());
    install_tui_features();
    #[cfg(feature = "session-index")]
    let session_index = {
        let index = pa_session_index::SessionIndex::new();
        pa_core::session::catalog_cache::install(Box::new(index.clone()));
        index
    };
    InstalledFeatures {
        #[cfg(feature = "session-index")]
        session_index,
        #[cfg(feature = "trace")]
        trace: pa_trace::install(pa_trace::RecorderConfig::from_env(
            trace_log_path(),
            crate::config::VERSION,
        ))
        .ok(),
    }
}

/// Install the enabled features' TUI seams before the first frame: (feature `mermaid`)
/// the fork's Mermaid diagrams as the TUI's diagram renderer. Idempotent; no I/O.
pub fn install_tui_features() {
    #[cfg(feature = "mermaid")]
    crate::mermaid_diagrams::install();
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

/// How long the process waits at exit for the features' background work.
const FEATURE_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Give the installed features a bounded chance to finish background work
/// before the process exits; returns at once when none is installed.
pub fn flush_enabled_features() {
    pa_core::features::flush_installed(FEATURE_FLUSH_TIMEOUT);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each Cargo feature installs its crate, and nothing else is installed:
    /// `--no-default-features` installs none.
    #[test]
    fn the_build_installs_exactly_its_enabled_features() {
        let names: Vec<&str> = enabled_features()
            .iter()
            .map(|feature| feature.name())
            .collect();
        let expected: Vec<&str> = vec![
            #[cfg(feature = "recall")]
            "recall",
            #[cfg(feature = "toolforge")]
            "toolforge",
            #[cfg(feature = "workflow")]
            "workflow",
            #[cfg(feature = "learning")]
            "learning",
            #[cfg(feature = "dream")]
            "dream",
            #[cfg(feature = "ravo")]
            "ravo",
            #[cfg(feature = "ledger")]
            "ledger",
        ];
        assert_eq!(names, expected);
    }
}
