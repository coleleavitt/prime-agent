//! Asking the user about the project servers that wait for approval
//! (Claude Code's "New MCP server found in this project" dialog), through
//! the session's confirmation surface ([`crate::features::confirm_offering`]):
//! Yes uses the server, the remembered choice uses it and every future
//! server of this project, No continues without it. The answer is recorded
//! in the choices store ([`super::record_project_choice`]) and the manager
//! re-resolves, so an approved server is usable at once. An unanswered
//! question (timed out, no client, print mode) leaves the server pending:
//! the next session or `/mcp` asks again.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use pa_types::sync::MutexExt;

use super::{DeclaredServer, ProjectChoice, record_project_choice};
use crate::features::{CONFIRMATION_TIMEOUT, ConfirmationAnswer, confirm_offering};
use crate::mcp::{McpManager, McpServerConfig};

/// What allowing for good remembers (the confirm panel's third choice).
const REMEMBER: &str = "MCP servers from this project's .mcp.json";

/// The questions being asked, by `(session, workspace, server)`: a second
/// asker for the same server waits for nothing and asks nothing.
fn in_flight() -> &'static Mutex<HashSet<(String, String, String)>> {
    static IN_FLIGHT: OnceLock<Mutex<HashSet<(String, String, String)>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Removes an in-flight entry when its question ends.
struct Asking((String, String, String));

impl Drop for Asking {
    fn drop(&mut self) {
        in_flight()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// What the server would run.
fn describe(server: &DeclaredServer) -> String {
    match &server.config {
        McpServerConfig::Stdio { command, args, .. } => {
            let mut line = command.clone();
            for arg in args.iter().flatten() {
                line.push(' ');
                line.push_str(arg);
            }
            format!("runs: {line}")
        }
        McpServerConfig::Http { url, .. } => format!("connects to: {url}"),
    }
}

/// The question's action and reason lines.
pub(super) fn question(server: &DeclaredServer) -> (String, String) {
    (
        format!(
            "New MCP server found in this project: {}\n{}",
            server.name,
            describe(server)
        ),
        format!(
            "Declared in {}. MCP servers may execute code or access system resources. \
             Yes uses this server; No continues without it.",
            server.source.display()
        ),
    )
}

/// The choice an answer records; `None` leaves the server pending.
pub(super) fn choice_of(answer: &ConfirmationAnswer) -> Option<ProjectChoice> {
    match answer {
        ConfirmationAnswer::Allowed => Some(ProjectChoice::Approve),
        ConfirmationAnswer::AllowedAlways => Some(ProjectChoice::ApproveAll),
        ConfirmationAnswer::Denied => Some(ProjectChoice::Reject),
        // A server approval is never put to a permission reviewer.
        ConfirmationAnswer::Unavailable
        | ConfirmationAnswer::TimedOut(_)
        | ConfirmationAnswer::Refused(_) => None,
    }
}

/// Ask `session_id`'s user about each project server of `manager`'s
/// workspace that waits for approval, one question at a time, recording
/// the answers. Returns once every question ended. A server already being
/// asked about in this session is skipped.
pub async fn ask_pending_project_servers(session_id: String, manager: Arc<Mutex<McpManager>>) {
    // Each server is asked about once per call, whatever its state after.
    let mut asked: HashSet<String> = HashSet::new();
    loop {
        let reader = Arc::clone(&manager);
        let Ok(declared) =
            tokio::task::spawn_blocking(move || reader.lock_or_recover().declared_servers()).await
        else {
            return;
        };
        let workspace = declared.workspace.display().to_string();
        let next = declared.pending().into_iter().find_map(|server| {
            if asked.contains(&server.name) {
                return None;
            }
            let key = (session_id.clone(), workspace.clone(), server.name.clone());
            let mut asking = in_flight().lock().unwrap_or_else(PoisonError::into_inner);
            asking
                .insert(key.clone())
                .then(|| (server.clone(), Asking(key)))
        });
        let Some((server, _asking)) = next else {
            return;
        };
        asked.insert(server.name.clone());
        let (action, reason) = question(&server);
        let answer = confirm_offering(
            &session_id,
            &action,
            &reason,
            Some(REMEMBER),
            CONFIRMATION_TIMEOUT,
        )
        .await;
        let Some(choice) = choice_of(&answer) else {
            // Unanswered: it stays pending, and so do the rest (the user
            // is not there to ask).
            return;
        };
        let agent_dir = declared.agent_dir.clone();
        let workspace = declared.workspace.clone();
        let refresh = Arc::clone(&manager);
        let recorded = tokio::task::spawn_blocking(move || {
            let recorded = record_project_choice(&agent_dir, &workspace, &server.name, choice);
            refresh.lock_or_recover().refresh();
            recorded
        })
        .await;
        match recorded {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(error = %format!("{error:#}"), "MCP server choice not recorded");
                return;
            }
            Err(_) => return,
        }
    }
}
