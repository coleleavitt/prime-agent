//! The interactive `--resume <selector>` and `/resume <selector>` resolve (Claude Code's
//! `--resume <value>`): an existing session file, then a saved session id (exact or a unique
//! prefix/suffix, this directory's sessions first), then a saved session's title (exact,
//! case-insensitive, this directory's sessions first). A selector that matches several saved
//! sessions opens the agents view searching for it; one that matches none attaches a running
//! session by that selector, and that attach's miss opens the same search.

use std::path::Path;

use pa_core::session::discovery::{
    ResolvedSession,
    SessionSelectorError,
    looks_like_session_path,
    resolve_session_path,
};
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
        [] => ResumeLaunch::Open(SessionSelection::Attach(selector.to_string())),
        [only] => ResumeLaunch::Open(SessionSelection::Resume(only.path.clone())),
        several => search(selector, several.len()),
    }
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
}
