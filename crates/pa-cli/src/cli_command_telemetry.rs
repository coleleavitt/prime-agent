//! `cli command used`: adoption of the `prime-agent` CLI surfaces that have
//! no event of their own — the named-session flags (`--name`,
//! `--list-sessions`, `--delete-session`), `prime-agent create`, and
//! `prime-agent trust --list`. The surface, its outcome, and (for `--name`)
//! whether it created the session: never a session name, id, or path.

use std::path::Path;

/// One CLI surface `cli command used` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliCommand {
    /// `--name <name>`; `created` is whether the launch created the session.
    Name {
        created: bool,
    },
    ListSessions,
    DeleteSession,
    Create,
    TrustList,
}

impl CliCommand {
    fn as_str(self) -> &'static str {
        match self {
            CliCommand::Name { .. } => "name",
            CliCommand::ListSessions => "list_sessions",
            CliCommand::DeleteSession => "delete_session",
            CliCommand::Create => "create",
            CliCommand::TrustList => "trust_list",
        }
    }
}

/// The event's properties (the base properties plus the catalogued three).
pub(crate) fn properties(command: CliCommand, ok: bool) -> pa_telemetry::Properties {
    let mut properties = pa_telemetry::base_properties("cli");
    properties.set("command", serde_json::Value::from(command.as_str()));
    properties.set(
        "outcome",
        serde_json::Value::from(if ok { "ok" } else { "error" }),
    );
    if let CliCommand::Name { created } = command {
        properties.set("created", serde_json::Value::from(created));
    }
    properties
}

/// How long a one-shot command waits at exit for the delivery.
const FLUSH_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Report one CLI command run. `wait` holds the caller for the bounded
/// delivery (a one-shot command exits right after); otherwise the delivery
/// runs on a detached thread and the launch continues at once.
pub(crate) fn report(cwd: &Path, command: CliCommand, ok: bool, wait: bool) {
    report_in(cwd, &crate::config::get_agent_dir(), command, ok, wait);
}

fn report_in(cwd: &Path, agent_dir: &Path, command: CliCommand, ok: bool, wait: bool) {
    let settings = pa_core::settings::SettingsManager::create(cwd, agent_dir);
    if crate::mode::telemetry_disabled(&settings) {
        return;
    }
    let properties = properties(command, ok);
    let agent_dir = agent_dir.to_path_buf();
    let worker = std::thread::Builder::new()
        .name("cli-command-telemetry".to_string())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                let client =
                    pa_core::session_engine::telemetry::build_client(&settings, &agent_dir);
                client.track("cli command used", properties);
                let _ = tokio::time::timeout(FLUSH_BUDGET, client.shutdown()).await;
            });
        });
    if wait {
        if let Ok(worker) = worker {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The event carries only its vocabulary: the surface, the outcome, and
    /// `created` for `--name` alone.
    #[test]
    fn the_event_carries_only_the_surface_and_its_outcome() {
        let mut expected = pa_telemetry::base_properties("cli");
        expected.set("command", serde_json::json!("name"));
        expected.set("outcome", serde_json::json!("ok"));
        expected.set("created", serde_json::json!(true));
        assert_eq!(
            properties(CliCommand::Name { created: true }, true),
            expected
        );
        let mut expected = pa_telemetry::base_properties("cli");
        expected.set("command", serde_json::json!("delete_session"));
        expected.set("outcome", serde_json::json!("error"));
        assert_eq!(properties(CliCommand::DeleteSession, false), expected);
    }

    /// A waited report lands (the local mirror shows it) before the call
    /// returns; an opted-out install sends nothing.
    #[test]
    fn a_waited_report_is_delivered_and_an_opt_out_sends_nothing() {
        crate::mode::tests::with_clean_telemetry_env(|| {
            let dir = tempfile::TempDir::new().unwrap();
            let agent_dir = dir.path().join("agent");
            std::fs::create_dir_all(&agent_dir).unwrap();
            report_in(dir.path(), &agent_dir, CliCommand::TrustList, true, true);
            let mirror = std::fs::read_to_string(agent_dir.join("telemetry.jsonl")).unwrap();
            let events: Vec<serde_json::Value> = mirror
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .filter(|event: &serde_json::Value| event["name"] == "cli command used")
                .collect();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["properties"]["command"], "trust_list");
            assert_eq!(events[0]["properties"]["outcome"], "ok");

            let off = dir.path().join("off");
            pa_core::settings::SettingsManager::create(dir.path(), &off)
                .set_telemetry_enabled(false)
                .unwrap();
            report_in(dir.path(), &off, CliCommand::Create, true, true);
            assert!(!off.join("telemetry.jsonl").exists());
        });
    }
}
