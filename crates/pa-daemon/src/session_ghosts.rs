//! The boot ghost sweep (upstream #1079 / #903): a fresh path-backed session writes its creation
//! prefix and `session_state: active` before any message, so a session the user opened and
//! never used leaves a file behind on every daemon exit. Once per boot, after adoption and the
//! update restore settled the resident roster, the supervisor removes those files.
//!
//! A ghost is judged conservatively; a file that holds anything a user could want is never
//! removed:
//! - only creation-prefix and lifecycle rows (`model_change`, `thinking_level_change`,
//!   `service_tier_change`, `session_state`), and no user content past the default prefix
//!   (a second model change is a user's switch);
//! - a top-level session (no parent, depth 0): a child belongs to its parent's ledger;
//! - not resident, not an active scheduled job's target, and not leased by a live process;
//! - untouched for [`GHOST_MIN_AGE`]: a concurrent daemon's fresh session is never a ghost;
//! - no artifact partition with files in it.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::lease::canonical_session_path;
use crate::session_store::SessionFile;

/// How long a ghost must sit untouched before the sweep removes it.
pub(crate) const GHOST_MIN_AGE: Duration = Duration::from_hours(1);

/// A ghost file is a header plus a handful of short rows; anything bigger is not judged.
const GHOST_MAX_BYTES: u64 = 64 * 1024;

/// The rows a fresh session writes before its first message.
const BOOTSTRAP_ENTRY_TYPES: [&str; 4] = [
    "model_change",
    "thinking_level_change",
    "service_tier_change",
    "session_state",
];

/// What a sweep may not touch, and the clock it judges age against.
pub(crate) struct GhostSweep<'a> {
    pub(crate) agent_dir: &'a Path,
    /// Canonical session paths of resident workers and active scheduled jobs.
    pub(crate) protected: &'a HashSet<PathBuf>,
    pub(crate) now: SystemTime,
}

impl GhostSweep<'_> {
    /// Remove every ghost directly under `sessions_dir`; returns the removed paths.
    pub(crate) fn sweep(&self, sessions_dir: &Path) -> Vec<PathBuf> {
        let Ok(read) = fs::read_dir(sessions_dir) else {
            return Vec::new();
        };
        let mut removed = Vec::new();
        for path in read.flatten().map(|entry| entry.path()) {
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            if !self.is_ghost(&path) {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => {
                    crate::saved_session_commands::remove_session_artifacts(&path);
                    removed.push(path);
                }
                Err(error) => {
                    eprintln!("ghost sweep: could not remove {}: {error}", path.display());
                }
            }
        }
        removed
    }

    fn is_ghost(&self, path: &Path) -> bool {
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        if !metadata.is_file() || metadata.len() > GHOST_MAX_BYTES {
            return false;
        }
        let young = metadata
            .modified()
            .ok()
            .and_then(|modified| self.now.duration_since(modified).ok())
            .is_none_or(|age| age < GHOST_MIN_AGE);
        if young || self.protected.contains(&canonical_session_path(path)) {
            return false;
        }
        if crate::lease::live_lease_owner(self.agent_dir, path).is_some() {
            return false;
        }
        let Ok(session) = SessionFile::open(path) else {
            return false;
        };
        let top_level =
            session.header.parent_session.is_none() && session.header.rlm_depth.unwrap_or(0) == 0;
        let bootstrap_only = session
            .entries()
            .iter()
            .all(|entry| BOOTSTRAP_ENTRY_TYPES.contains(&entry.type_.as_str()));
        top_level
            && bootstrap_only
            && !session.has_user_content()
            && !has_artifacts(path, session.session_id())
    }
}

/// The once-per-boot sweep: the caller runs it after adoption and the update restore, so every
/// resident session is in the protected set. Housekeeping only; it never gates serving.
pub(crate) async fn boot_ghost_sweep(supervisor: &std::sync::Arc<crate::supervisor::Supervisor>) {
    let agent_dir = supervisor.options.agent_dir.clone();
    let sessions_dir = match crate::paths::sessions_dir(&agent_dir) {
        Ok(dir) => dir,
        Err(error) => {
            supervisor.log_line(&format!("ghost session sweep failed: {error:#}"));
            return;
        }
    };
    let protected = crate::session_archive::protected_session_paths(supervisor).await;
    let removed = tokio::task::spawn_blocking(move || {
        GhostSweep {
            agent_dir: &agent_dir,
            protected: &protected,
            now: SystemTime::now(),
        }
        .sweep(&sessions_dir)
    })
    .await
    .unwrap_or_default();
    if !removed.is_empty() {
        supervisor.log_line(&format!(
            "removed {} unused empty session file(s)",
            removed.len()
        ));
    }
}

/// Whether the session's artifact partition holds anything (scheduled jobs, kernel state).
fn has_artifacts(path: &Path, session_id: &str) -> bool {
    crate::scheduled_jobs::session_artifact_dir(path, session_id)
        .and_then(|dir| fs::read_dir(dir).ok())
        .is_some_and(|mut entries| entries.next().is_some())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    struct Fixture {
        _root: tempfile::TempDir,
        agent_dir: PathBuf,
        sessions: PathBuf,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let agent_dir = root.path().join("agent");
        let sessions = agent_dir.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        Fixture {
            _root: root,
            agent_dir,
            sessions,
        }
    }

    /// A fresh create's file: the creation prefix plus `session_state: active`, then `edit`.
    fn session(dir: &Path, name: &str, edit: impl FnOnce(&mut SessionFile)) -> PathBuf {
        let mut file = SessionFile::create("/tmp", None, 0);
        file.append_model_change("prime-inference", "mock-1");
        file.append_thinking_level_change("off");
        file.append_entry("service_tier_change", json!({ "serviceTier": null }));
        file.append_session_state("active");
        edit(&mut file);
        let path = dir.join(format!("{name}.jsonl"));
        file.set_path(path.clone());
        file.rewrite().unwrap();
        path
    }

    fn age(path: &Path, by: Duration) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - by).unwrap();
    }

    #[test]
    fn the_boot_sweep_removes_only_old_unused_top_level_sessions() {
        let fixture = fixture();
        let old = GHOST_MIN_AGE + Duration::from_secs(60);
        let ghost = session(&fixture.sessions, "ghost", |_| {});
        let archived_ghost = session(&fixture.sessions, "archived-ghost", |file| {
            file.append_session_state("archived");
        });
        let with_message = session(&fixture.sessions, "with-message", |file| {
            file.append_message(&json!({ "role": "user", "content": "hi", "timestamp": 1u64 }));
        });
        let named = session(&fixture.sessions, "named", |file| {
            file.append_session_info("keep me");
        });
        let switched_model = session(&fixture.sessions, "switched-model", |file| {
            file.append_model_change("prime-inference", "mock-2");
        });
        let child = session(&fixture.sessions, "child", |file| {
            file.header.rlm_depth = Some(1);
        });
        let protected = session(&fixture.sessions, "protected", |_| {});
        let with_artifacts = session(&fixture.sessions, "with-artifacts", |_| {});
        let artifacts_id = SessionFile::open(&with_artifacts)
            .unwrap()
            .session_id()
            .to_string();
        let artifacts = fixture
            .agent_dir
            .join("session-artifacts")
            .join(&artifacts_id);
        fs::create_dir_all(&artifacts).unwrap();
        fs::write(artifacts.join("cron-jobs.json"), "[]").unwrap();
        for path in [
            &ghost,
            &archived_ghost,
            &with_message,
            &named,
            &switched_model,
            &child,
            &protected,
            &with_artifacts,
        ] {
            age(path, old);
        }
        let young = session(&fixture.sessions, "young", |_| {});

        let protected_paths = HashSet::from([canonical_session_path(&protected)]);
        let mut removed = GhostSweep {
            agent_dir: &fixture.agent_dir,
            protected: &protected_paths,
            now: SystemTime::now(),
        }
        .sweep(&fixture.sessions);
        removed.sort();

        assert_eq!(removed, vec![archived_ghost, ghost]);
        let mut left: Vec<PathBuf> = fs::read_dir(&fixture.sessions)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        left.sort();
        let mut expected = vec![
            with_message,
            named,
            switched_model,
            child,
            protected,
            with_artifacts,
            young,
        ];
        expected.sort();
        assert_eq!(left, expected);
    }
}
