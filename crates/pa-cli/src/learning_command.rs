//! `prime-agent learning` (pa-learning), composed behind the `learning`
//! feature: the command lives in `pa_learning::command`; this wiring
//! supplies stdout, stderr, the wall clock and the agent dir, and emits the
//! `learning_report` adoption event.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pa_learning::command::{run_learning_command, LearningCommandIo, LearningRunReport};

/// The process's stdout/stderr and the wall clock.
struct StdIo;

impl LearningCommandIo for StdIo {
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

/// How long the event may take to reach its sinks after the report is
/// printed (the command has finished; nothing waits on paint here).
const TELEMETRY_FLUSH_BUDGET: Duration = Duration::from_secs(2);

/// Run the command and return its exit code.
pub(crate) fn run(args: &[String]) -> i32 {
    let started = Instant::now();
    let outcome = run_learning_command(args, &crate::config::get_agent_dir(), &mut StdIo);
    if let Some(report) = outcome.report {
        track(&report, started.elapsed());
    }
    outcome.exit_code
}

/// The `learning_report` properties: vocabularies and counts only.
fn properties(report: &LearningRunReport, elapsed: Duration) -> pa_telemetry::Properties {
    let mut properties = pa_telemetry::base_properties("cli");
    properties.set(
        "subcommand",
        serde_json::Value::from(report.subcommand.as_str()),
    );
    properties.set("outcome", serde_json::Value::from(report.outcome.as_str()));
    properties.set("days", serde_json::Value::from(report.days));
    properties.set("sealed", serde_json::Value::from(report.sealed));
    properties.set("backfill", serde_json::Value::from(report.backfill));
    properties.set(
        "duration_ms",
        serde_json::Value::from(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
    );
    properties
}

/// Emit `learning_report` through the product client unless telemetry is
/// off. The client needs a runtime of its own; a helper thread hosts it so
/// a caller already inside one is never re-entered.
fn track(report: &LearningRunReport, elapsed: Duration) {
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
            client.track("learning_report", properties);
            let _ = tokio::time::timeout(TELEMETRY_FLUSH_BUDGET, client.shutdown()).await;
        });
    });
    let _ = worker.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_learning::command::{LearningOutcome, LearningSubcommand};

    #[test]
    fn the_event_carries_only_its_catalogued_vocabulary_and_counts() {
        let report = LearningRunReport {
            subcommand: LearningSubcommand::Trajectory,
            outcome: LearningOutcome::Withheld,
            days: 16,
            sealed: 2,
            backfill: true,
        };
        let mut properties = properties(&report, Duration::from_millis(40));
        assert_eq!(
            pa_telemetry::sanitize("learning_report", &mut properties),
            0
        );
        let expected = [
            ("subcommand", serde_json::json!("trajectory")),
            ("outcome", serde_json::json!("withheld")),
            ("days", serde_json::json!(16)),
            ("sealed", serde_json::json!(2)),
            ("backfill", serde_json::json!(true)),
            ("duration_ms", serde_json::json!(40)),
        ];
        for (key, value) in expected {
            assert_eq!(properties.get(key), Some(&value), "{key}");
        }
    }

    #[test]
    fn learning_is_a_public_command_with_its_help() {
        assert!(crate::command_registry::public_command_names().contains(&"learning"));
        let help =
            crate::command_registry::format_command_help(&["learning"]).expect("help learning");
        assert!(help.contains(pa_learning::command::LEARNING_USAGE));
        for row in pa_learning::command::LEARNING_OPTIONS {
            let flag = row.split_whitespace().next().unwrap_or_default();
            assert!(help.contains(flag), "{flag}");
        }
    }

    /// A run through the public dispatcher: usage errors exit 1, a sealed
    /// index reports, and the command reads the agent dir's log.
    #[test]
    fn the_dispatcher_routes_learning_and_exits_with_its_codes() {
        let _guard = crate::config::env_lock();
        let previous = std::env::var_os("DO_NOT_TRACK");
        std::env::set_var("DO_NOT_TRACK", "1");
        let dir = tempfile::TempDir::new().expect("temp dir");
        let index = dir.path().join("days");
        let args = |values: &[&str]| -> Vec<String> {
            values.iter().map(|value| (*value).to_string()).collect()
        };
        let index_arg = index.display().to_string();
        let results: Vec<(bool, Option<i32>)> = [
            args(&["learning", "--nope"]),
            args(&["learning", "--no-seal", "--index", &index_arg]),
            args(&["learning", "trajectory", "--bogus"]),
        ]
        .iter()
        .map(|values| {
            let result = crate::public_command::handle_public_command(values);
            (result.handled, result.exit_code)
        })
        .collect();
        match previous {
            Some(previous) => std::env::set_var("DO_NOT_TRACK", previous),
            None => std::env::remove_var("DO_NOT_TRACK"),
        }
        assert_eq!(
            results,
            vec![(true, Some(1)), (true, Some(1)), (true, Some(1))]
        );
    }
}
