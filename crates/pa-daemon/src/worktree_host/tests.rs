//! `worktree.enter` / `worktree.exit` over a recording seam and hermetic
//! fixture repositories (no global or system git config, `TZ=UTC`).

use std::sync::Mutex;

use pa_core::git_env::run_fixture_git;

use super::*;

/// The worker seam in memory: the branch entries it would persist.
struct RecordingSeam {
    cwd: Mutex<String>,
    entries: Mutex<Vec<Value>>,
}

impl RecordingSeam {
    fn new(cwd: &Path) -> Arc<Self> {
        Arc::new(Self {
            cwd: Mutex::new(cwd.display().to_string()),
            entries: Mutex::new(Vec::new()),
        })
    }

    fn cwd(&self) -> String {
        self.cwd.lock_or_recover().clone()
    }
}

impl WorktreeSession for RecordingSeam {
    fn view(&self) -> WorktreeSessionView {
        let entries = self.entries.lock_or_recover().clone();
        WorktreeSessionView {
            cwd: self.cwd(),
            session_id: "s1".to_string(),
            state: latest_state(entries.iter()),
        }
    }

    fn moved(&self, cwd: &str, state: &WorktreeSessionState) -> anyhow::Result<()> {
        cwd.clone_into(&mut self.cwd.lock_or_recover());
        self.entries.lock_or_recover().push(json!({
            "customType": WORKTREE_SESSION_STATE_CUSTOM_TYPE,
            "data": state,
        }));
        Ok(())
    }
}

fn fixture(temp: &tempfile::TempDir) -> (PathBuf, Arc<RecordingSeam>, WorktreeHostContext) {
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run_fixture_git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    run_fixture_git(&repo, &["add", "README.md"]);
    run_fixture_git(&repo, &["commit", "-q", "-m", "seed"]);
    let repo = std::fs::canonicalize(repo).unwrap();
    let seam = RecordingSeam::new(&repo);
    let context = WorktreeHostContext {
        seam: Arc::clone(&seam) as Arc<dyn WorktreeSession>,
        session: Arc::new(tokio::sync::Mutex::new(None)),
        agent_dir: temp.path().join("agent"),
    };
    (repo, seam, context)
}

#[tokio::test]
async fn enter_then_keep_moves_the_session_in_and_back_out() {
    let temp = tempfile::tempdir().unwrap();
    let (repo, seam, context) = fixture(&temp);

    let entered = context.enter(&json!({ "name": "feat" })).await.unwrap();
    let worktree = repo.join(".prime/worktrees/feat");
    assert_eq!(entered["worktreePath"], worktree.display().to_string());
    assert_eq!(entered["worktreeBranch"], "worktree-feat");
    assert_eq!(entered["existed"], false);
    assert_eq!(seam.cwd(), worktree.display().to_string());
    let state = seam.view().state.unwrap();
    assert!(state.active);
    assert_eq!(state.original_cwd, repo.display().to_string());

    // A second create while inside is refused; switching by path is not.
    let refused = context
        .enter(&json!({ "name": "other" }))
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .starts_with("Already in a worktree session")
    );

    let kept = context.exit(&json!({ "action": "keep" })).await.unwrap();
    assert_eq!(kept["restoredCwd"], repo.display().to_string());
    assert_eq!(seam.cwd(), repo.display().to_string());
    assert!(!seam.view().state.unwrap().active);
    assert!(worktree.is_dir(), "keep leaves the worktree on disk");

    // Nothing active now: exit is a no-op.
    let none = context.exit(&json!({ "action": "remove" })).await.unwrap();
    assert_eq!(none["action"], "none");
    assert!(worktree.is_dir());
}

#[tokio::test]
async fn remove_refuses_work_until_discarded_and_never_removes_an_entered_worktree() {
    let temp = tempfile::tempdir().unwrap();
    let (repo, seam, context) = fixture(&temp);

    context.enter(&json!({ "name": "work" })).await.unwrap();
    let worktree = repo.join(".prime/worktrees/work");
    std::fs::write(worktree.join("draft.txt"), "x").unwrap();
    let refused = context
        .exit(&json!({ "action": "remove" }))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refused.starts_with("Worktree has 1 uncommitted file."),
        "{refused}"
    );
    assert_eq!(seam.cwd(), worktree.display().to_string(), "still inside");

    let removed = context
        .exit(&json!({ "action": "remove", "discardChanges": true }))
        .await
        .unwrap();
    assert_eq!(removed["restoredCwd"], repo.display().to_string());
    let message = removed["message"].as_str().unwrap();
    assert!(
        message.starts_with(&format!(
            "Exited and removed worktree at {}. Discarded 1 uncommitted file(s).",
            worktree.display()
        )),
        "{message}"
    );
    assert!(!worktree.exists());

    // An existing worktree entered by path is the user's: never removed.
    let theirs = repo.join(".prime/worktrees/theirs");
    run_fixture_git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "theirs",
            &theirs.to_string_lossy(),
        ],
    );
    let entered = context
        .enter(&json!({ "path": theirs.display().to_string() }))
        .await
        .unwrap();
    assert_eq!(entered["existed"], true);
    let refused = context
        .exit(&json!({ "action": "remove", "discardChanges": true }))
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("not the owner"), "{refused}");
    assert!(theirs.is_dir());
}

#[tokio::test]
async fn bad_requests_are_refused_without_moving() {
    let temp = tempfile::tempdir().unwrap();
    let (repo, seam, context) = fixture(&temp);
    assert!(
        context
            .enter(&json!({ "name": "a", "path": "/x" }))
            .await
            .is_err()
    );
    assert!(context.enter(&json!({ "name": "../up" })).await.is_err());
    assert!(context.exit(&json!({ "action": "drop" })).await.is_err());
    assert_eq!(seam.cwd(), repo.display().to_string());
    assert_eq!(seam.view().state, None);
}
