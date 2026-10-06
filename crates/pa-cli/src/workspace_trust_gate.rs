//! Workspace trust at the composition root: the launch gate (the
//! interactive one-time question, `--trust-workspace`, the headless
//! notice) and the `trust` / `untrust` commands. The decision itself and
//! its enforcement live in `pa_core::workspace_trust`; daemon workers read
//! the recorded decision and never ask.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use pa_core::workspace_trust::{self, TrustDecision, TrustState, WorkspaceTrustStatus};

use crate::mode::AppMode;

/// Where a recorded decision came from (the event's `source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecisionSource {
    Prompt,
    Flag,
    Command,
}

impl DecisionSource {
    fn as_str(self) -> &'static str {
        match self {
            DecisionSource::Prompt => "prompt",
            DecisionSource::Flag => "flag",
            DecisionSource::Command => "command",
        }
    }
}

/// What the launch gate does for a workspace that is not trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchAction {
    /// Trusted, nothing to gate, or the supervisor (no workspace).
    Proceed,
    /// `--trust-workspace`: record trust, then proceed.
    RecordTrust,
    /// The interactive terminal asks once.
    Ask,
    /// Headless modes (and interactive without a terminal to ask on)
    /// print the notice and run without the project configuration.
    Notice,
    /// The interactive user already declined this content: run without it.
    Quiet,
}

fn launch_action(
    status: &WorkspaceTrustStatus,
    app_mode: AppMode,
    trust_flag: bool,
    can_ask: bool,
) -> LaunchAction {
    if app_mode == AppMode::Daemon || status.is_trusted() {
        return LaunchAction::Proceed;
    }
    if trust_flag {
        return LaunchAction::RecordTrust;
    }
    match app_mode {
        AppMode::Interactive if status.needs_decision() && can_ask => LaunchAction::Ask,
        AppMode::Interactive if !status.needs_decision() => LaunchAction::Quiet,
        AppMode::Interactive | AppMode::Print | AppMode::Json | AppMode::Rpc | AppMode::Acp => {
            LaunchAction::Notice
        }
        AppMode::Daemon => LaunchAction::Proceed,
    }
}

/// The launch gate: run before any session starts for `cwd`.
///
/// # Errors
///
/// Returns the message to print when `--trust-workspace` cannot record
/// the decision (the run must not proceed as if it were trusted).
pub(crate) fn gate_launch(
    cwd: &Path,
    agent_dir: &Path,
    app_mode: AppMode,
    trust_flag: bool,
) -> Result<(), String> {
    let status = workspace_trust::evaluate(cwd, agent_dir);
    let can_ask = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    match launch_action(&status, app_mode, trust_flag, can_ask) {
        LaunchAction::Proceed | LaunchAction::Quiet => Ok(()),
        LaunchAction::RecordTrust => {
            workspace_trust::record(cwd, agent_dir, TrustDecision::Trusted)
                .map_err(|error| format!("--trust-workspace: {error:#}"))?;
            track(DecisionSource::Flag, TrustDecision::Trusted, &status);
            Ok(())
        }
        LaunchAction::Ask => {
            let answer = ask(
                &status,
                &mut std::io::stdin().lock(),
                &mut std::io::stderr(),
            );
            if let Some(decision) = answer {
                if let Err(error) = workspace_trust::record(cwd, agent_dir, decision) {
                    eprintln!("Warning: could not record the workspace trust decision: {error:#}");
                }
                track(DecisionSource::Prompt, decision, &status);
            }
            Ok(())
        }
        LaunchAction::Notice => {
            if let Some(notice) = status.notice() {
                eprintln!("{notice}");
            }
            Ok(())
        }
    }
}

/// Ask once on the terminal. `y`/`yes` trusts; any other answer declines
/// (and is remembered); end of input decides nothing.
fn ask(
    status: &WorkspaceTrustStatus,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Option<TrustDecision> {
    let heading = if status.state == TrustState::Changed {
        "The project configuration of this workspace changed since you last decided. It can run code or change the agent's instructions:"
    } else {
        "This workspace has project configuration that can run code or change the agent's instructions:"
    };
    let _ = writeln!(output, "{heading}");
    for item in &status.gated {
        let _ = writeln!(output, "  - {item}");
    }
    let _ = write!(
        output,
        "Only trust workspaces whose contents you know.\nTrust {}? [y/N] ",
        status.workspace.display()
    );
    let _ = output.flush();
    let mut answer = String::new();
    match input.read_line(&mut answer) {
        Ok(0) | Err(_) => {
            let _ = writeln!(output);
            None
        }
        Ok(_) => Some(match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => TrustDecision::Trusted,
            _ => TrustDecision::Denied,
        }),
    }
}

/// Emit `workspace_trust_decision` without holding the caller: the launch
/// path hands the delivery to a detached thread and returns at once.
fn track(source: DecisionSource, decision: TrustDecision, status: &WorkspaceTrustStatus) {
    let mut properties = pa_telemetry::base_properties("cli");
    properties.set("source", serde_json::Value::from(source.as_str()));
    properties.set(
        "decision",
        serde_json::Value::from(match decision {
            TrustDecision::Trusted => "trusted",
            TrustDecision::Denied => "denied",
        }),
    );
    properties.set(
        "content_changed",
        serde_json::Value::from(status.state == TrustState::Changed),
    );
    let cwd = status.workspace.clone();
    spawn_detached(move || {
        let agent_dir = crate::config::get_agent_dir();
        let settings = pa_core::settings::SettingsManager::create(&cwd, &agent_dir);
        if crate::mode::telemetry_disabled(&settings) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async {
            let client = pa_core::session_engine::telemetry::build_client(&settings, &agent_dir);
            client.track("workspace_trust_decision", properties);
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(2), client.shutdown()).await;
        });
    });
}

/// The fire-and-forget boundary: the work runs on its own thread, never
/// joined, so a sink that never answers cannot hold the launch.
fn spawn_detached(work: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("workspace-trust-telemetry".to_string())
        .spawn(work);
}

/// `prime-agent trust [path] [--list]`.
pub(crate) fn run_trust_command(args: &[String]) -> i32 {
    let agent_dir = crate::config::get_agent_dir();
    if args.iter().any(|arg| arg == "--list") {
        if args.len() != 1 {
            eprintln!("Error: --list takes no path");
            return 1;
        }
        let code = match workspace_trust::list(&agent_dir) {
            Ok(records) if records.is_empty() => {
                println!("No workspace trust decisions recorded.");
                0
            }
            Ok(records) => {
                for (workspace, record) in records {
                    let decision = match record.decision {
                        TrustDecision::Trusted => "trusted",
                        TrustDecision::Denied => "denied ",
                    };
                    println!("{decision}  {workspace}");
                }
                0
            }
            Err(error) => {
                eprintln!("Error: {error:#}");
                1
            }
        };
        crate::cli_command_telemetry::report(
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            crate::cli_command_telemetry::CliCommand::TrustList,
            code == 0,
            true,
        );
        return code;
    }
    decide_from_command(args, &agent_dir, TrustDecision::Trusted)
}

/// `prime-agent untrust [path]`: record a denial (the next launch does not
/// ask again until the configuration changes).
pub(crate) fn run_untrust_command(args: &[String]) -> i32 {
    decide_from_command(args, &crate::config::get_agent_dir(), TrustDecision::Denied)
}

fn command_workspace(args: &[String]) -> Result<PathBuf, String> {
    if let Some(flag) = args.iter().find(|arg| arg.starts_with('-')) {
        return Err(format!("unknown option {flag:?}"));
    }
    match args {
        [] => std::env::current_dir().map_err(|error| error.to_string()),
        [path] => {
            let path = crate::config::expand_tilde_path(path);
            if path.is_dir() {
                Ok(path)
            } else {
                Err(format!("not a directory: {}", path.display()))
            }
        }
        _ => Err("expected at most one path".to_string()),
    }
}

fn decide_from_command(args: &[String], agent_dir: &Path, decision: TrustDecision) -> i32 {
    let cwd = match command_workspace(args) {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    };
    let before = workspace_trust::evaluate(&cwd, agent_dir);
    match workspace_trust::record(&cwd, agent_dir, decision) {
        Ok(status) => {
            println!("{}", command_summary(&status, decision));
            track(DecisionSource::Command, decision, &before);
            0
        }
        Err(error) => {
            eprintln!("Error: {error:#}");
            1
        }
    }
}

fn command_summary(status: &WorkspaceTrustStatus, decision: TrustDecision) -> String {
    let workspace = status.workspace.display();
    let verb = match decision {
        TrustDecision::Trusted => "Trusted",
        TrustDecision::Denied => "Not trusted",
    };
    if status.gated.is_empty() {
        return format!(
            "{verb}: {workspace} (it has no project configuration that needs trust yet)."
        );
    }
    let effect = match decision {
        TrustDecision::Trusted => "New sessions here load",
        TrustDecision::Denied => "New sessions here skip",
    };
    let items = status
        .gated
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    format!("{verb}: {workspace}. {effect}: {items}.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_core::workspace_trust::GatedItem;

    fn status(state: TrustState) -> WorkspaceTrustStatus {
        WorkspaceTrustStatus {
            workspace: PathBuf::from("/work/repo"),
            state,
            gated: vec![
                GatedItem::SettingsKeys(vec!["shellPath".to_string()]),
                GatedItem::SystemPrompt,
            ],
        }
    }

    #[test]
    fn headless_modes_get_the_notice_and_interactive_asks_once() {
        let unknown = status(TrustState::Unknown);
        for mode in [AppMode::Print, AppMode::Json, AppMode::Rpc, AppMode::Acp] {
            assert_eq!(
                launch_action(&unknown, mode, false, true),
                LaunchAction::Notice,
                "{mode:?}"
            );
            assert_eq!(
                launch_action(&unknown, mode, true, false),
                LaunchAction::RecordTrust,
                "{mode:?}"
            );
        }
        assert_eq!(
            launch_action(&unknown, AppMode::Interactive, false, true),
            LaunchAction::Ask
        );
        assert_eq!(
            launch_action(
                &status(TrustState::Changed),
                AppMode::Interactive,
                false,
                true
            ),
            LaunchAction::Ask
        );
        assert_eq!(
            launch_action(
                &status(TrustState::Denied),
                AppMode::Interactive,
                false,
                true
            ),
            LaunchAction::Quiet
        );
        assert_eq!(
            launch_action(&unknown, AppMode::Interactive, false, false),
            LaunchAction::Notice
        );
        assert_eq!(
            launch_action(&unknown, AppMode::Daemon, false, true),
            LaunchAction::Proceed
        );
        assert_eq!(
            launch_action(&status(TrustState::Trusted), AppMode::Print, false, false),
            LaunchAction::Proceed
        );
    }

    #[test]
    fn the_question_lists_the_gated_items_and_reads_one_answer() {
        let mut output = Vec::new();
        let answer = ask(
            &status(TrustState::Unknown),
            &mut std::io::Cursor::new("y\n"),
            &mut output,
        );
        assert_eq!(answer, Some(TrustDecision::Trusted));
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "This workspace has project configuration that can run code or change the agent's instructions:\n  \
             - .prime/agent/settings.json keys: shellPath\n  \
             - .prime/agent/SYSTEM.md\n\
             Only trust workspaces whose contents you know.\n\
             Trust /work/repo? [y/N] "
        );
        let mut sink = Vec::new();
        for (input, expected) in [
            ("yes\n", Some(TrustDecision::Trusted)),
            ("\n", Some(TrustDecision::Denied)),
            ("no\n", Some(TrustDecision::Denied)),
            ("", None),
        ] {
            assert_eq!(
                ask(
                    &status(TrustState::Changed),
                    &mut std::io::Cursor::new(input),
                    &mut sink
                ),
                expected,
                "{input:?}"
            );
        }
    }

    #[test]
    fn a_hanging_delivery_never_holds_the_caller() {
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let (finished, done) = std::sync::mpsc::channel::<()>();
        spawn_detached(move || {
            // A sink that never answers until released.
            let _ = hold.recv();
            let _ = finished.send(());
        });
        // The caller is back before the work could have finished.
        assert!(done.try_recv().is_err());
        release.send(()).unwrap();
        done.recv().unwrap();
    }

    #[test]
    fn the_command_summary_names_what_new_sessions_load() {
        assert_eq!(
            command_summary(&status(TrustState::Trusted), TrustDecision::Trusted),
            "Trusted: /work/repo. New sessions here load: .prime/agent/settings.json keys: shellPath; .prime/agent/SYSTEM.md."
        );
        let empty = WorkspaceTrustStatus {
            gated: Vec::new(),
            ..status(TrustState::NotRequired)
        };
        assert_eq!(
            command_summary(&empty, TrustDecision::Denied),
            "Not trusted: /work/repo (it has no project configuration that needs trust yet)."
        );
    }
}
