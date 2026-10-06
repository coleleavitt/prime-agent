//! Named sessions (upstream #1294): `--name <name>` opens the current directory's saved session
//! of that name and creates it when none exists, `--list-sessions` lists the directory's saved
//! sessions, and `--delete-session <id|name>` deletes one through the daemon. A session's name is
//! its `session_info` name — the field `/name`, `prime-agent rename`, and the agents view's rename
//! already write — so no separate name mapping exists.

use std::path::{Path, PathBuf};

use pa_daemon::session_store::{list_sessions, SessionFile, SessionInfo};

use crate::daemon_session_list::{format_session_age, format_session_display_id, format_table};

/// The current directory's top-level saved sessions, newest first.
fn sessions_here(session_dir: &Path, cwd: &Path) -> Vec<SessionInfo> {
    list_sessions(session_dir)
        .into_iter()
        .filter(|info| info.rlm_depth == 0 && Path::new(&info.cwd) == cwd)
        .collect()
}

/// The current directory's saved session named `name` (trimmed, exact).
///
/// # Errors
///
/// More than one session of the directory carries the name.
fn find_named_session(
    session_dir: &Path,
    cwd: &Path,
    name: &str,
) -> Result<Option<PathBuf>, String> {
    let name = name.trim();
    let mut matches: Vec<PathBuf> = sessions_here(session_dir, cwd)
        .into_iter()
        .filter(|info| info.name.as_deref().map(str::trim) == Some(name))
        .map(|info| info.path)
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        count => Err(format!(
            "Ambiguous session name \"{name}\": {count} sessions in this directory carry it. Use --resume <id> instead."
        )),
    }
}

/// `--name <name>`: the session file the launch opens — the directory's session of that name,
/// or a new session file created with that name (the open's create writes its prefix).
///
/// # Errors
///
/// An empty or ambiguous name, or a failed write of the new session file.
pub(crate) fn open_or_create_named_session(
    session_dir: &Path,
    cwd: &Path,
    name: &str,
) -> Result<NamedSessionOpen, String> {
    if name.trim().is_empty() {
        return Err("--name requires a non-empty session name".to_string());
    }
    if let Some(path) = find_named_session(session_dir, cwd, name)? {
        return Ok(NamedSessionOpen {
            path,
            created: false,
        });
    }
    let mut file = SessionFile::create(&cwd.to_string_lossy(), None, 0);
    file.append_session_info(name);
    let path = session_dir.join(pa_daemon::session_store::session_file_name(
        file.session_id(),
    ));
    file.set_path(path.clone());
    file.rewrite()
        .map_err(|error| format!("Cannot create session \"{}\": {error:#}", name.trim()))?;
    Ok(NamedSessionOpen {
        path,
        created: true,
    })
}

/// The session file a `--name` launch opens, and whether it was created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NamedSessionOpen {
    pub(crate) path: PathBuf,
    pub(crate) created: bool,
}

/// `--list-sessions`: the directory's saved sessions as an `ID / MODIFIED / NAME` table (the
/// id column is the display id every selector accepts).
pub(crate) fn format_sessions_here(session_dir: &Path, cwd: &Path, now_ms: u64) -> String {
    let rows: Vec<[String; 3]> = sessions_here(session_dir, cwd)
        .iter()
        .map(|info| {
            [
                format_session_display_id(&info.id),
                format_session_age(Some(&info.modified), now_ms),
                info.name.clone().unwrap_or_default(),
            ]
        })
        .collect();
    if rows.is_empty() {
        return format!("No saved sessions for {}.", cwd.display());
    }
    format_table(&["ID", "MODIFIED", "NAME"], &rows)
}

/// `--delete-session <id|name>`: a name of the directory's sessions first, else the saved-session
/// selector `--resume` takes (a path, a full id, or an id suffix).
///
/// # Errors
///
/// An ambiguous name, or a selector that matches no session (or several).
pub(crate) fn resolve_delete_target(
    selector: &str,
    session_dir: &Path,
    cwd: &Path,
) -> Result<PathBuf, String> {
    if let Some(path) = find_named_session(session_dir, cwd, selector)? {
        return Ok(path);
    }
    let resolved = pa_core::session::discovery::resolve_session_path(selector, cwd, session_dir)
        .map_err(|error| crate::print_runtime::render_selector_error(&error))?;
    match resolved {
        pa_core::session::discovery::ResolvedSession::Path(path)
        | pa_core::session::discovery::ResolvedSession::Local(path)
        | pa_core::session::discovery::ResolvedSession::Global { path, .. } => {
            if path.is_file() {
                Ok(path)
            } else {
                Err(format!("Session not found: {selector}"))
            }
        }
    }
}

/// `--delete-session <id|name>`: resolve the target, then delete it through the daemon (started
/// when absent), which refuses a session a live worker hosts and removes the session's
/// artifacts with it.
///
/// # Errors
///
/// An unresolvable selector, an unreachable daemon, or the daemon's refusal.
pub(crate) fn run_delete_session(
    selector: &str,
    session_dir: &Path,
    cwd: &Path,
    socket_path: &Path,
) -> Result<i32, String> {
    let path = resolve_delete_target(selector, session_dir, cwd)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime
        .block_on(crate::interactive_mode::ensure_daemon_running(
            socket_path,
            cwd,
        ))
        .map_err(|error| format!("{error:#}"))?;
    let mut client = crate::daemon_client::DaemonClient::connect(socket_path)
        .map_err(|error| format!("{error:#}"))?;
    let response = client
        .request(pa_types::daemon::DaemonCommand::DeleteSavedSession {
            id: None,
            active_session_id: None,
            session_path: path.to_string_lossy().to_string(),
            rest: serde_json::Map::new(),
        })
        .map_err(|error| format!("{error:#}"))?;
    if !response.success {
        return Err(response.error.unwrap_or_default());
    }
    let data = response.data.unwrap_or_default();
    if data.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        println!("Deleted session {}", path.display());
        return Ok(0);
    }
    Err(data
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("the session could not be deleted")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn saved(dir: &Path, cwd: &Path, name: Option<&str>) -> PathBuf {
        let mut file = SessionFile::create(&cwd.to_string_lossy(), None, 0);
        file.append_message(&json!({ "role": "user", "content": "hi", "timestamp": 1u64 }));
        if let Some(name) = name {
            file.append_session_info(name);
        }
        let path = dir.join(pa_daemon::session_store::session_file_name(
            file.session_id(),
        ));
        file.set_path(path.clone());
        file.rewrite().unwrap();
        path
    }

    #[test]
    fn a_name_resolves_in_the_current_directory_and_creates_once() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let here = root.path().join("here");
        let elsewhere = root.path().join("elsewhere");
        let named = saved(&sessions, &here, Some("work"));
        // The same name in another directory never matches here.
        saved(&sessions, &elsewhere, Some("notes"));

        assert_eq!(
            open_or_create_named_session(&sessions, &here, "work"),
            Ok(NamedSessionOpen {
                path: named.clone(),
                created: false
            })
        );
        let opened = open_or_create_named_session(&sessions, &here, "notes").unwrap();
        assert!(opened.created);
        let created = opened.path;
        assert_ne!(created, named);
        assert_eq!(
            SessionFile::open(&created).unwrap().header.cwd,
            here.to_string_lossy()
        );
        // The second open finds the session the first one created.
        assert_eq!(
            open_or_create_named_session(&sessions, &here, " notes "),
            Ok(NamedSessionOpen {
                path: created.clone(),
                created: false
            })
        );
        saved(&sessions, &here, Some("work"));
        assert_eq!(
            open_or_create_named_session(&sessions, &here, "work"),
            Err("Ambiguous session name \"work\": 2 sessions in this directory carry it. Use --resume <id> instead.".to_string())
        );
        assert_eq!(
            resolve_delete_target("notes", &sessions, &here),
            Ok(created.clone())
        );
        let id = SessionFile::open(&created)
            .unwrap()
            .session_id()
            .to_string();
        assert_eq!(resolve_delete_target(&id, &sessions, &here), Ok(created));
    }

    #[test]
    fn the_listing_shows_only_the_current_directory() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let here = root.path().join("here");
        let named = saved(&sessions, &here, Some("work"));
        saved(&sessions, &root.path().join("elsewhere"), Some("other"));
        let id = SessionFile::open(&named).unwrap().session_id().to_string();
        let listing = format_sessions_here(&sessions, &here, crate::daemon_session_list::now_ms());
        let lines: Vec<&str> = listing.lines().collect();
        assert_eq!(lines.len(), 2, "{listing}");
        assert!(lines[0].starts_with("ID"), "{listing}");
        assert!(
            lines[1].starts_with(&format_session_display_id(&id)),
            "{listing}"
        );
        assert!(lines[1].ends_with("work"), "{listing}");
        assert_eq!(
            format_sessions_here(&sessions, &root.path().join("empty"), 0),
            format!(
                "No saved sessions for {}.",
                root.path().join("empty").display()
            )
        );
    }
}
