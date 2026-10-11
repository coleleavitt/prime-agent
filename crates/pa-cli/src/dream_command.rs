//! `prime-agent dream` (pa-dream), composed behind the `dream` feature: the
//! command itself lives in `pa_dream::command`; this wiring supplies stdout,
//! stderr and the wall clock, and emits the `dream_run` adoption event.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pa_dream::command::{DreamCommandIo, DreamRunReport, run_dream_command};

/// The process's stdout/stderr and the wall clock, frozen per run by the command.
struct StdIo;

impl DreamCommandIo for StdIo {
    fn stdout(&mut self, line: &str) {
        println!("{line}");
    }

    fn stderr(&mut self, line: &str) {
        eprintln!("{line}");
    }

    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            })
    }
}

/// How long the event may take to reach its sinks after the run's output is
/// printed (the command has finished; nothing waits on paint here).
const TELEMETRY_FLUSH_BUDGET: Duration = Duration::from_secs(2);

/// Run the command and return its exit code.
pub(crate) fn run(args: &[String]) -> i32 {
    let started = Instant::now();
    let outcome = run_dream_command(args, &mut StdIo);
    if let Some(report) = outcome.report {
        track(&report, started.elapsed());
    }
    outcome.exit_code
}

/// The `dream_run` properties: vocabularies and counts only.
fn properties(report: &DreamRunReport, elapsed: Duration) -> pa_telemetry::Properties {
    let mut properties = pa_telemetry::base_properties("cli");
    properties.set(
        "subcommand",
        serde_json::Value::from(report.subcommand.as_str()),
    );
    properties.set("task", serde_json::Value::from(report.task.as_str()));
    properties.set("outcome", serde_json::Value::from(report.outcome.as_str()));
    properties.set("rollouts", serde_json::Value::from(report.rollouts));
    properties.set("probes", serde_json::Value::from(report.probes));
    properties.set("improved", serde_json::Value::from(report.improved));
    properties.set(
        "duration_ms",
        serde_json::Value::from(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
    );
    properties
}

/// Emit `dream_run` through the product client unless telemetry is off. The
/// client needs a runtime of its own; a helper thread hosts it so a caller
/// already inside one is never re-entered.
fn track(report: &DreamRunReport, elapsed: Duration) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let agent_dir = crate::config::get_agent_dir();
    let settings = pa_core::settings::SettingsManager::create(&cwd, &agent_dir);
    if crate::mode::telemetry_disabled(&settings) {
        return;
    }
    let properties = properties(report, elapsed);
    let worker = std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async {
            let client = pa_core::session_engine::telemetry::build_client(&settings, &agent_dir);
            client.track("dream_run", properties);
            let _ = tokio::time::timeout(TELEMETRY_FLUSH_BUDGET, client.shutdown()).await;
        });
    });
    let _ = worker.join();
}

#[cfg(test)]
mod tests {
    use pa_dream::command::{DreamRunOutcome, DreamSubcommand};
    use pa_dream::tasks::DreamTaskId;

    use super::*;

    #[test]
    fn the_event_carries_only_its_catalogued_vocabulary_and_counts() {
        let report = DreamRunReport {
            subcommand: DreamSubcommand::Experiment,
            task: DreamTaskId::Autocorrelation,
            outcome: DreamRunOutcome::Completed,
            rollouts: 8,
            probes: 131,
            improved: true,
        };
        let mut properties = properties(&report, Duration::from_millis(2_400));
        assert_eq!(pa_telemetry::sanitize("dream_run", &mut properties), 0);
        let expected = [
            ("subcommand", serde_json::json!("experiment")),
            ("task", serde_json::json!("autocorrelation")),
            ("outcome", serde_json::json!("completed")),
            ("rollouts", serde_json::json!(8)),
            ("probes", serde_json::json!(131)),
            ("improved", serde_json::json!(true)),
            ("duration_ms", serde_json::json!(2_400)),
        ];
        for (key, value) in expected {
            assert_eq!(properties.get(key), Some(&value), "{key}");
        }
    }

    #[test]
    fn dream_is_a_public_command_with_its_help() {
        assert!(crate::command_registry::public_command_names().contains(&"dream"));
        let help = crate::command_registry::format_command_help(&["dream"]).expect("help dream");
        assert!(help.contains(pa_dream::command::DREAM_USAGE));
        for row in pa_dream::command::DREAM_OPTIONS {
            let flag = row.split_whitespace().next().unwrap_or_default();
            assert!(help.contains(flag), "{flag}");
        }
    }
}
