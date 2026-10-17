//! The interactive `--resume <selector>` and `/resume <selector>` resolve (Claude Code's
//! `--resume <value>`): an existing session file, then a saved session id (exact or a unique
//! prefix/suffix, this directory's sessions first), then a saved session's title (exact,
//! case-insensitive, this directory's sessions first). A selector that matches several saved
//! sessions opens the agents view searching for it. One that matches none tries the installed
//! external session sources (a Claude Code session id or exact title, imported into a new
//! session file that resumes), then attaches a running session by that selector, and that
//! attach's miss opens the same search.

use std::path::Path;

use pa_core::session::discovery::{
    ResolvedSession,
    SessionSelectorError,
    looks_like_session_path,
    resolve_session_path,
};
use pa_core::session::external_sessions::{self, ExternalMatch, ImportedSession};
use pa_daemon::session_store::{SessionInfo, list_sessions};
use pa_tui::interactive::SessionSelection;

use crate::config;

/// Where a resume selector leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResumeLaunch {
    /// Open this session (a saved file, or a running session by its selector).
    Open(SessionSelection),
    /// Several saved sessions match: the agents view opens searching for the selector.
    Search { query: String, notice: String },
}

/// Resolve one resume selector against the saved sessions in `session_dir`.
pub(crate) fn resolve_resume_launch(
    selector: &str,
    cwd: &Path,
    session_dir: &Path,
) -> ResumeLaunch {
    let selector = selector.trim();
    let path = config::expand_tilde_path(selector);
    if path.is_file() {
        return ResumeLaunch::Open(SessionSelection::Resume(path));
    }
    let candidate = session_dir.join(format!("{selector}.jsonl"));
    if candidate.is_file() {
        return ResumeLaunch::Open(SessionSelection::Resume(candidate));
    }
    if selector.is_empty() || looks_like_session_path(selector) {
        return ResumeLaunch::Open(SessionSelection::Attach(selector.to_string()));
    }
    match resolve_session_path(selector, cwd, session_dir) {
        Ok(
            ResolvedSession::Path(path)
            | ResolvedSession::Local(path)
            | ResolvedSession::Global { path, .. },
        ) => return ResumeLaunch::Open(SessionSelection::Resume(path)),
        Err(SessionSelectorError::Ambiguous { matches, .. }) => {
            return search(selector, matches.len());
        }
        Err(SessionSelectorError::NotFound { .. }) => {}
    }
    let sessions = list_sessions(session_dir);
    let titled = sessions_titled(&sessions, selector, cwd);
    match titled.as_slice() {
        [] => {}
        [only] => return ResumeLaunch::Open(SessionSelection::Resume(only.path.clone())),
        several => return search(selector, several.len()),
    }
    // Another product's session (a Claude Code session id or title): imported into a new
    // session file, which resumes.
    match import_external(selector, cwd, session_dir) {
        None => ResumeLaunch::Open(SessionSelection::Attach(selector.to_string())),
        Some(Ok(imported)) => ResumeLaunch::Open(SessionSelection::Resume(imported.path)),
        Some(Err(notice)) => ResumeLaunch::Search {
            query: selector.to_string(),
            notice,
        },
    }
}

/// Resolve `selector` against the installed external session sources (Claude Code's
/// transcripts) and import the one it names into `session_dir`. `None` when no source
/// knows it; `Err` carries the notice for several matches or a failed import.
pub(crate) fn import_external(
    selector: &str,
    cwd: &Path,
    session_dir: &Path,
) -> Option<Result<ImportedSession, String>> {
    let (source, found) = external_sessions::find(selector, cwd)?;
    let label = source.label().to_string();
    Some(match found {
        ExternalMatch::None => return None,
        ExternalMatch::Many(count) => Err(format!(
            "{count} {label} sessions match \"{selector}\" — pick one."
        )),
        ExternalMatch::One(row) => source
            .import(&row.id, session_dir)
            .map_err(|error| format!("Could not import the {label} session: {error}")),
    })
}

fn search(selector: &str, matches: usize) -> ResumeLaunch {
    ResumeLaunch::Search {
        query: selector.to_string(),
        notice: format!("{matches} saved sessions match \"{selector}\" — pick one."),
    }
}

/// The saved sessions whose title (the `session_info` name) equals `title` ignoring case and
/// surrounding whitespace: this directory's when any match here, else every directory's.
pub(crate) fn sessions_titled<'a>(
    sessions: &'a [SessionInfo],
    title: &str,
    cwd: &Path,
) -> Vec<&'a SessionInfo> {
    let wanted = title.trim().to_lowercase();
    let named: Vec<&SessionInfo> = sessions
        .iter()
        .filter(|info| {
            info.name
                .as_deref()
                .is_some_and(|name| name.trim().to_lowercase() == wanted)
        })
        .collect();
    let here: Vec<&SessionInfo> = named
        .iter()
        .copied()
        .filter(|info| pa_types::platform::same_directory(Path::new(&info.cwd), cwd))
        .collect();
    if here.is_empty() { named } else { here }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pa_daemon::session_store::{SessionFile, session_file_name};

    use super::*;

    fn save(dir: &Path, cwd: &Path, name: Option<&str>) -> (String, PathBuf) {
        let mut session = SessionFile::create(&cwd.to_string_lossy(), None, 0);
        if let Some(name) = name {
            session.append_session_info(name);
        }
        session.append_message(&serde_json::json!({
            "role": "user", "content": "hi", "timestamp": 1u64
        }));
        let path = dir.join(session_file_name(session.session_id()));
        session.set_path(path.clone());
        session.rewrite().expect("write session");
        (session.session_id().to_string(), path)
    }

    struct Fixture {
        _root: tempfile::TempDir,
        sessions: PathBuf,
        here: PathBuf,
        elsewhere: PathBuf,
    }

    fn fixture() -> Fixture {
        let root = tempfile::TempDir::new().expect("temp dir");
        let sessions = root.path().join("sessions");
        let here = root.path().join("here");
        let elsewhere = root.path().join("elsewhere");
        for dir in [&sessions, &here, &elsewhere] {
            std::fs::create_dir_all(dir).expect("dir");
        }
        Fixture {
            _root: root,
            sessions,
            here,
            elsewhere,
        }
    }

    #[test]
    fn a_saved_session_resumes_by_an_id_prefix_or_suffix() {
        let fx = fixture();
        let (id, path) = save(&fx.sessions, &fx.here, None);
        let compact = id.replace('-', "");
        for selector in [&id[..8], &compact[compact.len() - 12..]] {
            assert_eq!(
                resolve_resume_launch(selector, &fx.here, &fx.sessions),
                ResumeLaunch::Open(SessionSelection::Resume(path.clone())),
                "{selector}"
            );
        }
    }

    #[test]
    fn a_title_resumes_ignoring_case_and_prefers_this_directory() {
        let fx = fixture();
        let (_, here) = save(&fx.sessions, &fx.here, Some("Fix the parser"));
        save(&fx.sessions, &fx.elsewhere, Some("fix the parser"));
        assert_eq!(
            resolve_resume_launch("  FIX the Parser ", &fx.here, &fx.sessions),
            ResumeLaunch::Open(SessionSelection::Resume(here))
        );
        let (_, other) = save(&fx.sessions, &fx.elsewhere, Some("Only elsewhere"));
        assert_eq!(
            resolve_resume_launch("only elsewhere", &fx.here, &fx.sessions),
            ResumeLaunch::Open(SessionSelection::Resume(other))
        );
    }

    #[test]
    fn several_matches_open_the_search() {
        let fx = fixture();
        save(&fx.sessions, &fx.here, Some("refactor"));
        save(&fx.sessions, &fx.here, Some("Refactor"));
        assert_eq!(
            resolve_resume_launch("refactor", &fx.here, &fx.sessions),
            ResumeLaunch::Search {
                query: "refactor".to_string(),
                notice: "2 saved sessions match \"refactor\" — pick one.".to_string(),
            }
        );
    }

    #[test]
    fn no_match_attaches_by_the_selector() {
        let fx = fixture();
        save(&fx.sessions, &fx.here, Some("something else"));
        assert_eq!(
            resolve_resume_launch("missing words", &fx.here, &fx.sessions),
            ResumeLaunch::Open(SessionSelection::Attach("missing words".to_string()))
        );
    }

    /// A Claude Code session id or title resumes through an import into the sessions dir;
    /// the transcript stays as it was.
    #[cfg(feature = "cc-import")]
    #[test]
    fn a_claude_code_session_resumes_through_its_import() {
        let fx = fixture();
        let claude = fx.here.with_file_name("claude-projects");
        let id = "9a8b7c6d-1111-4222-8333-444455556666";
        let project = pa_cc_import::project_dir(&claude, &fx.here);
        std::fs::create_dir_all(&project).expect("project dir");
        let transcript = project.join(format!("{id}.jsonl"));
        let cwd = fx.here.to_string_lossy();
        let lines = [
            serde_json::json!({
                "type": "user", "uuid": "u1", "parentUuid": null, "isSidechain": false,
                "cwd": cwd, "sessionId": id, "timestamp": "2026-03-01T10:00:00.000Z",
                "message": { "role": "user", "content": "hello from claude code" },
            }),
            serde_json::json!({ "type": "custom-title", "customTitle": "CC Chat", "sessionId": id }),
        ];
        let text = format!("{}\n{}\n", lines[0], lines[1]);
        std::fs::write(&transcript, &text).expect("transcript");
        pa_core::session::external_sessions::install(std::sync::Arc::new(
            pa_cc_import::ClaudeCodeSessions { root: Some(claude) },
        ));
        let ResumeLaunch::Open(SessionSelection::Resume(path)) =
            resolve_resume_launch(id, &fx.here, &fx.sessions)
        else {
            panic!("the Claude Code id resumes its import");
        };
        assert_eq!(path.parent(), Some(fx.sessions.as_path()));
        assert_eq!(
            resolve_resume_launch("cc chat", &fx.here, &fx.sessions),
            ResumeLaunch::Open(SessionSelection::Resume(path)),
            "the title finds the same, unchanged session: its import is reused"
        );
        assert_eq!(std::fs::read_to_string(&transcript).expect("read"), text);
    }
}
