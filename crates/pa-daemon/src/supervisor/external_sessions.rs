//! Other agent products' sessions (capability `external_sessions`): the
//! `list_external_sessions` listing and the `import_external_session`
//! conversion, both served from the sources the composition root installed
//! (`pa_core::session::external_sessions`). Both run on the blocking pool:
//! a listing reads transcript files, an import writes a session file.

use pa_core::session::external_sessions::{
    self,
    ExternalScope,
    ExternalSessionRow,
    ImportedSession,
};

use super::{DaemonCommand, DaemonResponse, Supervisor, Value, json, paths};
use crate::protocol::{response_failure, response_success};

/// One listing row on the wire.
pub(crate) fn external_session_row(row: &ExternalSessionRow) -> Value {
    let mut value = json!({
        "source": row.source,
        "sourceLabel": row.source_label,
        // The id the agents view keys the row by; the source's own id rides
        // `externalId`.
        "id": format!("{}:{}", row.source, row.id),
        "externalId": row.id,
        "path": row.path.to_string_lossy(),
        "cwd": row.cwd,
        "created": row.created,
        "modified": row.modified,
        "messageCount": row.message_count,
    });
    let object = value.as_object_mut().expect("row object");
    if let Some(title) = &row.title {
        object.insert("name".to_string(), json!(title));
    }
    if let Some(prompt) = &row.first_prompt {
        object.insert("firstMessage".to_string(), json!(prompt));
    }
    if let Some(branch) = &row.git_branch {
        object.insert("gitBranch".to_string(), json!(branch));
    }
    value
}

fn imported_reply(imported: &ImportedSession) -> Value {
    json!({
        "sessionPath": imported.path.to_string_lossy(),
        "sessionId": imported.session_id,
        "cwd": imported.cwd,
        "reused": imported.reused,
    })
}

impl Supervisor {
    pub(super) async fn handle_list_external_sessions(
        &self,
        command: &DaemonCommand,
        command_id: &str,
        type_name: &str,
    ) -> DaemonResponse {
        let DaemonCommand::ListExternalSessions { cwd, all, .. } = command else {
            return response_failure(Some(command_id), type_name, "malformed command", None);
        };
        let scope = if all.unwrap_or(false) {
            ExternalScope::All
        } else {
            match cwd {
                Some(cwd) => match paths::expand_tilde(cwd) {
                    Ok(cwd) => ExternalScope::Directory(cwd),
                    Err(error) => {
                        return response_failure(
                            Some(command_id),
                            type_name,
                            &error.to_string(),
                            None,
                        );
                    }
                },
                None => {
                    return response_failure(
                        Some(command_id),
                        type_name,
                        "list_external_sessions needs a cwd or all",
                        None,
                    );
                }
            }
        };
        let listed = tokio::task::spawn_blocking(move || external_sessions::list_all(&scope)).await;
        match listed {
            Ok(rows) => {
                let sessions: Vec<Value> = rows.iter().map(external_session_row).collect();
                response_success(
                    Some(command_id),
                    type_name,
                    Some(json!({ "sessions": sessions })),
                )
            }
            Err(error) => response_failure(
                Some(command_id),
                type_name,
                &format!("the external-session scan failed: {error}"),
                None,
            ),
        }
    }

    pub(super) async fn handle_import_external_session(
        &self,
        command: &DaemonCommand,
        command_id: &str,
        type_name: &str,
    ) -> DaemonResponse {
        let DaemonCommand::ImportExternalSession {
            source,
            external_id,
            session_dir,
            ..
        } = command
        else {
            return response_failure(Some(command_id), type_name, "malformed command", None);
        };
        let dir = match session_dir.as_deref() {
            Some(dir) => paths::expand_tilde(dir),
            None => paths::sessions_dir(&self.options.agent_dir),
        };
        let dir = match dir {
            Ok(dir) => dir,
            Err(error) => {
                return response_failure(Some(command_id), type_name, &error.to_string(), None);
            }
        };
        let (source, external_id) = (source.clone(), external_id.clone());
        let imported = tokio::task::spawn_blocking(move || {
            external_sessions::import(&source, &external_id, &dir)
        })
        .await;
        match imported {
            Ok(Ok(imported)) => {
                response_success(Some(command_id), type_name, Some(imported_reply(&imported)))
            }
            Ok(Err(error)) => response_failure(Some(command_id), type_name, &error, None),
            Err(error) => response_failure(
                Some(command_id),
                type_name,
                &format!("the import failed: {error}"),
                None,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use pa_core::session::external_sessions::{ExternalMatch, ExternalSessionSource};

    use super::*;

    /// A source that lists one session per scope and imports into the
    /// given dir (the only source this test binary installs).
    struct Stub;

    impl ExternalSessionSource for Stub {
        fn source(&self) -> &'static str {
            "stub-source"
        }

        fn label(&self) -> &'static str {
            "Stub"
        }

        fn list(&self, scope: &ExternalScope) -> Vec<ExternalSessionRow> {
            let id = match scope {
                ExternalScope::All => "everywhere".to_string(),
                ExternalScope::Directory(cwd) => format!("in {}", cwd.display()),
            };
            vec![ExternalSessionRow {
                source: "stub-source".to_string(),
                source_label: "Stub".to_string(),
                id,
                path: PathBuf::from("/stub.jsonl"),
                cwd: "/w".to_string(),
                title: Some("T".to_string()),
                first_prompt: None,
                created: "2026-01-01T00:00:00.000Z".to_string(),
                modified: "2026-01-01T00:00:00.000Z".to_string(),
                git_branch: Some("main".to_string()),
                message_count: 1,
            }]
        }

        fn find(&self, _selector: &str, _cwd: &Path) -> ExternalMatch {
            ExternalMatch::None
        }

        fn import(&self, id: &str, session_dir: &Path) -> Result<ImportedSession, String> {
            if id == "bad" {
                return Err("Unknown stub session bad".to_string());
            }
            Ok(ImportedSession {
                path: session_dir.join(format!("{id}.jsonl")),
                session_id: id.to_string(),
                cwd: "/w".to_string(),
                reused: false,
            })
        }
    }

    #[tokio::test]
    async fn the_listing_and_the_import_serve_the_installed_sources() {
        external_sessions::install(Arc::new(Stub));
        assert!(
            crate::protocol::default_server_capabilities()
                .contains(&crate::protocol::EXTERNAL_SESSIONS_CAPABILITY.to_string())
        );
        let dir = tempfile::TempDir::new().unwrap();
        let supervisor = Supervisor::new(super::super::SupervisorOptions {
            tcp_port: None,
            tcp_bind_host: None,
            remote_agent_mesh: None,
            socket_path: dir.path().join("daemon.sock"),
            agent_dir: dir.path().join("agent"),
        })
        .expect("supervisor");
        let command = |value: Value| -> DaemonCommand { serde_json::from_value(value).unwrap() };

        let listed = supervisor
            .handle_list_external_sessions(
                &command(json!({ "type": "list_external_sessions", "cwd": "/w/repo" })),
                "c1",
                "list_external_sessions",
            )
            .await;
        assert!(listed.success, "{listed:?}");
        let sessions = &listed.data.as_ref().unwrap()["sessions"];
        assert_eq!(sessions[0]["externalId"], "in /w/repo");
        assert_eq!(sessions[0]["id"], "stub-source:in /w/repo");
        let all = supervisor
            .handle_list_external_sessions(
                &command(json!({ "type": "list_external_sessions", "all": true })),
                "c2",
                "list_external_sessions",
            )
            .await;
        assert_eq!(all.data.unwrap()["sessions"][0]["externalId"], "everywhere");
        let neither = supervisor
            .handle_list_external_sessions(
                &command(json!({ "type": "list_external_sessions" })),
                "c3",
                "list_external_sessions",
            )
            .await;
        assert!(!neither.success);

        let session_dir = dir.path().join("sessions");
        let imported = supervisor
            .handle_import_external_session(
                &command(json!({
                    "type": "import_external_session", "source": "stub-source",
                    "externalId": "abc", "sessionDir": session_dir.to_string_lossy(),
                })),
                "c4",
                "import_external_session",
            )
            .await;
        assert!(imported.success, "{imported:?}");
        assert_eq!(
            imported.data.unwrap(),
            json!({
                "sessionPath": session_dir.join("abc.jsonl").to_string_lossy(),
                "sessionId": "abc",
                "cwd": "/w",
                "reused": false,
            })
        );
        // Without a session dir the import lands in the daemon's sessions dir.
        let default_dir = supervisor
            .handle_import_external_session(
                &command(json!({
                    "type": "import_external_session", "source": "stub-source", "externalId": "d",
                })),
                "c5",
                "import_external_session",
            )
            .await;
        let path = default_dir.data.unwrap()["sessionPath"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            Path::new(&path).starts_with(dir.path().join("agent")),
            "{path}"
        );
        for (source, id, error) in [
            ("stub-source", "bad", "Unknown stub session bad"),
            ("nope", "x", "Unknown session source \"nope\""),
        ] {
            let failed = supervisor
                .handle_import_external_session(
                    &command(json!({
                        "type": "import_external_session", "source": source, "externalId": id,
                    })),
                    "c6",
                    "import_external_session",
                )
                .await;
            assert_eq!(failed.error.as_deref(), Some(error));
        }
    }

    #[test]
    fn a_row_keys_by_source_and_keeps_absent_fields_absent() {
        let row = ExternalSessionRow {
            source: "claude-code".to_string(),
            source_label: "Claude Code".to_string(),
            id: "abc".to_string(),
            path: PathBuf::from("/p/abc.jsonl"),
            cwd: "/w".to_string(),
            title: None,
            first_prompt: Some("fix it".to_string()),
            created: "2026-01-01T00:00:00.000Z".to_string(),
            modified: "2026-01-02T00:00:00.000Z".to_string(),
            git_branch: None,
            message_count: 4,
        };
        assert_eq!(
            external_session_row(&row),
            json!({
                "source": "claude-code",
                "sourceLabel": "Claude Code",
                "id": "claude-code:abc",
                "externalId": "abc",
                "path": "/p/abc.jsonl",
                "cwd": "/w",
                "created": "2026-01-01T00:00:00.000Z",
                "modified": "2026-01-02T00:00:00.000Z",
                "messageCount": 4,
                "firstMessage": "fix it",
            })
        );
    }
}
