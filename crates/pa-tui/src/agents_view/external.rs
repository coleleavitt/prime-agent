//! Other agent products' sessions in the agents view (Claude Code's
//! transcripts), behind the daemon's `external_sessions` capability: the
//! rows come from `list_external_sessions` (every project's, filtered like the
//! saved catalog by the project and branch toggles) and list in their own
//! section after the saved sessions; opening one asks the daemon to
//! `import_external_session` it into a new session file, which then resumes
//! like any saved session.

use std::path::PathBuf;

use pa_types::daemon::DaemonCommand;
use serde_json::Value;
use tokio::sync::mpsc;

use super::{AgentsViewMode, AgentsViewRow, SessionSelection, UiInput};
use crate::daemon_client::DaemonClient;

/// The daemon capability that serves the listing and the import.
pub(super) const EXTERNAL_SESSIONS_CAPABILITY: &str = "external_sessions";

/// One import the run loop dispatches: the row it opens once the daemon
/// wrote the session file.
#[derive(Debug, Clone)]
pub(super) struct ImportRequest {
    pub(super) row: AgentsViewRow,
    pub(super) ancestors: Vec<String>,
    pub(super) source: String,
    pub(super) external_id: String,
    pub(super) label: String,
    pub(super) session_dir: Option<PathBuf>,
}

fn field<'a>(summary: &'a Value, key: &str) -> Option<&'a str> {
    summary
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// Fetch every project's external sessions. A failure leaves the section
/// empty: the saved catalog is the view's own data, these rows are extra.
pub(super) fn spawn_external_catalog_fetch(
    client: &DaemonClient,
    ui_tx: mpsc::UnboundedSender<UiInput>,
) {
    let client = client.clone();
    tokio::spawn(async move {
        let response = client
            .request_with_timeout(
                DaemonCommand::ListExternalSessions {
                    id: None,
                    cwd: None,
                    all: Some(true),
                    rest: serde_json::Map::default(),
                },
                crate::daemon_client::LONG_RUNNING_REQUEST_TIMEOUT_MS,
            )
            .await;
        let Ok(response) = response else {
            return;
        };
        if !response.success {
            return;
        }
        let sessions = response
            .data
            .as_ref()
            .and_then(|data| data.get("sessions"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let _ = ui_tx.send(UiInput::ExternalLoaded { sessions });
    });
}

/// Run one import off the key loop; its outcome lands as an `ImportResult`.
pub(super) fn spawn_import_dispatch(
    client: &DaemonClient,
    ui_tx: mpsc::UnboundedSender<UiInput>,
    request: ImportRequest,
) -> tokio::task::JoinHandle<()> {
    let client = client.clone();
    tokio::spawn(async move {
        let response = client
            .request_with_timeout(
                DaemonCommand::ImportExternalSession {
                    id: None,
                    source: request.source.clone(),
                    external_id: request.external_id.clone(),
                    session_dir: request
                        .session_dir
                        .as_ref()
                        .map(|dir| dir.to_string_lossy().to_string()),
                    rest: serde_json::Map::default(),
                },
                crate::daemon_client::LONG_RUNNING_REQUEST_TIMEOUT_MS,
            )
            .await;
        let outcome = match response {
            Ok(response) if response.success => response
                .data
                .as_ref()
                .and_then(|data| data.get("sessionPath"))
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .ok_or_else(|| "the import answered no session path".to_string()),
            Ok(response) => Err(response.error.unwrap_or_else(|| "unknown error".into())),
            Err(error) => Err(error.to_string()),
        };
        let _ = ui_tx.send(UiInput::ImportResult {
            request: Box::new(request),
            outcome,
        });
    })
}

impl AgentsViewMode {
    /// The external rows landed: they join the list after the saved ones.
    pub(super) fn apply_external_loaded(&mut self, sessions: Vec<Value>) {
        self.external = sessions
            .into_iter()
            .filter(crate::agents_view_state::is_external_row)
            .collect();
        self.rebuild_rows();
    }

    /// Open an external row: queue its import (one at a time). `false` when
    /// the row is not an external one.
    pub(super) fn request_import(&mut self, row: &AgentsViewRow, ancestors: Vec<String>) -> bool {
        let summary = &row.summary;
        let (Some(source), Some(external_id)) = (
            field(summary, "externalSource"),
            field(summary, "externalId"),
        ) else {
            return false;
        };
        let label = field(summary, "sourceLabel").unwrap_or(source).to_string();
        if self.import_in_flight {
            self.set_status(&format!("Still importing the {label} session..."));
            return true;
        }
        self.import_in_flight = true;
        self.set_status(&format!("Importing the {label} session..."));
        self.pending_import = Some(ImportRequest {
            row: row.clone(),
            ancestors,
            source: source.to_string(),
            external_id: external_id.to_string(),
            label,
            session_dir: self.options.session_dir.clone(),
        });
        true
    }

    /// One import landed: open the new session file, or report the failure.
    pub(super) fn import_result(
        &mut self,
        request: &ImportRequest,
        outcome: Result<PathBuf, String>,
    ) {
        self.import_in_flight = false;
        match outcome {
            Ok(path) => {
                self.status = None;
                self.actions.push("external_session_imported");
                self.finish_open(
                    SessionSelection::Resume(path),
                    &request.row,
                    request.ancestors.clone(),
                    None,
                    None,
                );
            }
            Err(error) => self.set_status_tone(
                &format!("Could not import the {} session: {error}", request.label),
                super::StatusTone::Error,
            ),
        }
    }

    /// The label the external section's heading shows: the rows' own source
    /// label when they agree on one.
    pub(super) fn external_section_label(&self) -> Option<String> {
        let mut labels = self
            .rows
            .iter()
            .filter(|row| row.section == crate::agents_view_state::Section::External)
            .filter_map(|row| field(&row.summary, "sourceLabel"));
        let first = labels.next()?;
        labels
            .all(|label| label == first)
            .then(|| first.to_string())
    }
}
