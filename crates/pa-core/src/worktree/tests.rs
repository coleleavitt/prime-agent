//! The worktree helper over hermetic fixture repositories (no global or
//! system git config, `TZ=UTC`, a fixed identity: `git_env::fixture_git`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::git_env::run_fixture_git;

/// A repository with one commit on `main`, its canonical root.
fn fixture_repo(temp: &tempfile::TempDir) -> PathBuf {
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run_fixture_git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.join(".gitignore"), ".env\nbuild/\nconfig/local.toml\n").unwrap();
    run_fixture_git(&repo, &["add", "README.md", ".gitignore"]);
    run_fixture_git(&repo, &["commit", "-q", "-m", "seed"]);
    std::fs::canonicalize(repo).unwrap()
}

fn request(from: &Path, name: &str) -> CreateRequest {
    CreateRequest {
        name: Some(name.to_string()),
        from: from.to_path_buf(),
        base: BaseRef::Head,
        hooks: None,
    }
}

#[test]
fn create_makes_a_branch_and_a_checkout_under_the_managed_dir() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    let head = run_fixture_git(&repo, &["rev-parse", "HEAD"]);

    let worktree = create(&request(&repo, "feature/login")).unwrap();

    assert_eq!(
        worktree,
        Worktree {
            name: "feature/login".to_string(),
            path: repo.join(".prime/worktrees/feature+login"),
            branch: Some("worktree-feature+login".to_string()),
            repo_root: Some(repo.clone()),
            base_commit: Some(head.clone()),
            existed: false,
            hook_based: false,
            copied: Vec::new(),
        }
    );
    assert!(worktree.path.join("README.md").is_file());
    assert_eq!(read_base_marker(&worktree.path), Some(head));
    assert!(is_registered(&repo, &worktree.path));
    assert!(is_managed_path(&repo, &worktree.path));
    // The managed directory never shows in the main checkout's status.
    assert_eq!(run_fixture_git(&repo, &["status", "--porcelain"]), "");
}

#[test]
fn creating_from_inside_a_worktree_resolves_the_main_repository() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    let first = create(&request(&repo, "one")).unwrap();

    assert_eq!(main_repo_root(&first.path).unwrap(), repo);
    let second = create(&request(&first.path, "two")).unwrap();
    assert_eq!(second.path, repo.join(".prime/worktrees/two"));
}

#[test]
fn an_existing_worktree_is_reused_with_its_recorded_base() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    let first = create(&request(&repo, "again")).unwrap();
    std::fs::write(first.path.join("work.txt"), "x").unwrap();
    run_fixture_git(&first.path, &["add", "work.txt"]);
    run_fixture_git(&first.path, &["commit", "-q", "-m", "work"]);

    let second = create(&request(&repo, "again")).unwrap();

    assert!(second.existed);
    assert_eq!(second.path, first.path);
    assert_eq!(second.branch.as_deref(), Some("worktree-again"));
    assert_eq!(second.base_commit, first.base_commit);
    assert_eq!(
        changes(&second.path, second.base_commit.as_deref()).unwrap(),
        WorktreeChanges {
            changed_files: 0,
            commits_ahead: 1,
        }
    );
}

#[test]
fn an_existing_branch_is_checked_out_never_reset() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    run_fixture_git(&repo, &["branch", "worktree-kept"]);
    run_fixture_git(&repo, &["checkout", "-q", "worktree-kept"]);
    std::fs::write(repo.join("kept.txt"), "x").unwrap();
    run_fixture_git(&repo, &["add", "kept.txt"]);
    run_fixture_git(&repo, &["commit", "-q", "-m", "kept"]);
    let kept_tip = run_fixture_git(&repo, &["rev-parse", "HEAD"]);
    run_fixture_git(&repo, &["checkout", "-q", "main"]);

    let worktree = create(&request(&repo, "kept")).unwrap();

    assert_eq!(
        run_fixture_git(&worktree.path, &["rev-parse", "HEAD"]),
        kept_tip
    );
    assert_eq!(worktree.base_commit, Some(kept_tip));
}

#[test]
fn a_stray_directory_or_symlink_at_the_managed_path_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    std::fs::create_dir_all(repo.join(".prime/worktrees/stray")).unwrap();
    assert!(matches!(
        create(&request(&repo, "stray")),
        Err(WorktreeError::NotAWorktree { .. })
    ));

    #[cfg(unix)]
    {
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join(".prime/worktrees/linked")).unwrap();
        assert!(matches!(
            create(&request(&repo, "linked")),
            Err(WorktreeError::Symlink(_))
        ));
    }
}

#[test]
fn outside_git_without_a_hook_is_an_error_and_bad_names_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let plain = temp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert!(matches!(
        create(&request(&plain, "x")),
        Err(WorktreeError::NotARepository(_))
    ));
    let repo = fixture_repo(&temp);
    assert!(matches!(
        create(&request(&repo, "../escape")),
        Err(WorktreeError::InvalidName(_))
    ));
}

#[test]
fn worktreeinclude_copies_matching_ignored_files_only() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
    std::fs::create_dir_all(repo.join("config")).unwrap();
    std::fs::write(repo.join("config/local.toml"), "x = 1\n").unwrap();
    std::fs::create_dir_all(repo.join("build")).unwrap();
    std::fs::write(repo.join("build/out.bin"), "big").unwrap();
    std::fs::write(repo.join("untracked.txt"), "not ignored").unwrap();
    std::fs::write(
        repo.join(WORKTREE_INCLUDE_FILE),
        "# copied into new worktrees\n.env\nconfig/local.toml\nuntracked.txt\n",
    )
    .unwrap();

    let worktree = create(&request(&repo, "with-env")).unwrap();

    let mut copied = worktree.copied.clone();
    copied.sort();
    assert_eq!(copied, vec![".env", "config/local.toml"]);
    assert_eq!(
        std::fs::read_to_string(worktree.path.join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert!(!worktree.path.join("build/out.bin").exists());
    // Not ignored: a file the user has not ignored is never copied.
    assert!(!worktree.path.join("untracked.txt").exists());
    // Ignored copies do not make the worktree dirty.
    assert!(
        changes(&worktree.path, worktree.base_commit.as_deref())
            .unwrap()
            .is_clean()
    );
}

#[test]
fn remove_keeps_a_worktree_with_work_unless_discarding() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    let worktree = create(&request(&repo, "busy")).unwrap();
    std::fs::write(worktree.path.join("new.txt"), "x").unwrap();

    assert_eq!(
        remove(&worktree, false, None).unwrap(),
        RemoveOutcome::Kept(WorktreeChanges {
            changed_files: 1,
            commits_ahead: 0,
        })
    );
    assert!(worktree.path.exists());

    assert_eq!(
        remove(&worktree, true, None).unwrap(),
        RemoveOutcome::Removed {
            branch_deleted: true
        }
    );
    assert!(!worktree.path.exists());
    assert!(!run_fixture_git(&repo, &["branch", "--list"]).contains("worktree-busy"));
}

#[test]
fn removing_a_clean_worktree_deletes_its_branch_only_when_no_commit_is_lost() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    let clean = create(&request(&repo, "clean")).unwrap();
    assert_eq!(
        remove(&clean, false, None).unwrap(),
        RemoveOutcome::Removed {
            branch_deleted: true
        }
    );
    assert!(!clean.path.exists());

    // The branch's only commit predates this worktree (a reused branch):
    // the worktree is clean, but deleting the branch would lose the commit.
    run_fixture_git(&repo, &["branch", "worktree-orphan"]);
    run_fixture_git(&repo, &["checkout", "-q", "worktree-orphan"]);
    std::fs::write(repo.join("only-here.txt"), "x").unwrap();
    run_fixture_git(&repo, &["add", "only-here.txt"]);
    run_fixture_git(&repo, &["commit", "-q", "-m", "only here"]);
    run_fixture_git(&repo, &["checkout", "-q", "main"]);
    let reused = create(&request(&repo, "orphan")).unwrap();
    assert_eq!(
        remove(&reused, false, None).unwrap(),
        RemoveOutcome::Removed {
            branch_deleted: false
        }
    );
    assert!(run_fixture_git(&repo, &["branch", "--list"]).contains("worktree-orphan"));
}

#[test]
fn remove_refuses_a_path_outside_the_managed_directory() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    let mut worktree = create(&request(&repo, "real")).unwrap();
    worktree.path = repo.clone();
    assert!(matches!(
        remove(&worktree, true, None),
        Err(WorktreeError::NotAWorktree { .. })
    ));
    assert!(repo.join("README.md").exists());
}

#[test]
fn fresh_base_uses_the_fetched_default_branch() {
    let temp = tempfile::tempdir().unwrap();
    let upstream = fixture_repo(&temp);
    let clone = temp.path().join("clone");
    run_fixture_git(
        temp.path(),
        &[
            "clone",
            "-q",
            &upstream.to_string_lossy(),
            &clone.to_string_lossy(),
        ],
    );
    let origin_main = run_fixture_git(&clone, &["rev-parse", "origin/main"]);
    std::fs::write(clone.join("local.txt"), "x").unwrap();
    run_fixture_git(&clone, &["add", "local.txt"]);
    run_fixture_git(&clone, &["commit", "-q", "-m", "local only"]);

    let worktree = create(&CreateRequest {
        base: BaseRef::Fresh,
        ..request(&clone, "fresh")
    })
    .unwrap();

    assert_eq!(worktree.base_commit, Some(origin_main));
    assert!(!worktree.path.join("local.txt").exists());
}

struct FakeHooks {
    dir: PathBuf,
}

impl WorktreeHooks for FakeHooks {
    fn create(
        &self,
        _context: &WorktreeHookContext,
        name: &str,
    ) -> Option<Result<PathBuf, String>> {
        let path = self.dir.join(name);
        Some(
            std::fs::create_dir_all(&path)
                .map(|()| path)
                .map_err(|error| error.to_string()),
        )
    }

    fn remove(&self, _context: &WorktreeHookContext, path: &Path) -> Option<Result<(), String>> {
        Some(std::fs::remove_dir_all(path).map_err(|error| error.to_string()))
    }
}

#[test]
fn a_create_hook_replaces_git_and_works_outside_a_repository() {
    let temp = tempfile::tempdir().unwrap();
    let plain = temp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let made = temp.path().join("made");
    // The only test that installs hooks: every other test passes no hook
    // context, so the process-global provider never reaches them.
    install_hooks(Arc::new(FakeHooks { dir: made.clone() }));
    let context = WorktreeHookContext {
        cwd: plain.clone(),
        agent_dir: temp.path().join("agent"),
        session_id: Some("s1".to_string()),
    };

    let worktree = create(&CreateRequest {
        hooks: Some(context.clone()),
        ..request(&plain, "hooked")
    })
    .unwrap();

    assert!(worktree.hook_based);
    assert_eq!(worktree.path, made.join("hooked"));
    assert_eq!(
        remove(&worktree, false, Some(&context)).unwrap(),
        RemoveOutcome::Removed {
            branch_deleted: false
        }
    );
    assert!(!made.join("hooked").exists());
}

#[test]
fn changes_describe_reads_like_claude_codes_refusal() {
    let found = WorktreeChanges {
        changed_files: 2,
        commits_ahead: 1,
    };
    assert_eq!(
        found.describe(Some("worktree-x")),
        "2 uncommitted files and 1 commit on worktree-x"
    );
    assert_eq!(WorktreeChanges::default().describe(None), "no changes");
}

#[test]
fn only_a_clean_worktree_this_session_made_is_offered_for_removal() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    let made = create(&request(&repo, "exit")).unwrap();
    assert_eq!(exit_choice(&made, false), ExitChoice::OfferRemoval);
    assert!(matches!(exit_choice(&made, true), ExitChoice::Keep { .. }));

    std::fs::write(made.path.join("dirty.txt"), "x").unwrap();
    assert_eq!(
        exit_choice(&made, false),
        ExitChoice::Keep {
            reason: "it has 1 uncommitted file".to_string()
        }
    );
    let reused = create(&request(&repo, "exit")).unwrap();
    std::fs::remove_file(made.path.join("dirty.txt")).unwrap();
    assert_eq!(
        exit_choice(&reused, false),
        ExitChoice::Keep {
            reason: "it existed before this session".to_string()
        }
    );
}

#[test]
fn the_base_ref_setting_defaults_to_fresh() {
    let settings = |value: serde_json::Value| {
        let mut settings = crate::settings::Settings::default();
        if let serde_json::Value::Object(map) = value {
            settings.extra = map;
        }
        settings
    };
    assert_eq!(
        configured_base(&settings(serde_json::json!({}))),
        BaseRef::Fresh
    );
    assert_eq!(
        configured_base(&settings(
            serde_json::json!({"worktree": {"baseRef": "head"}})
        )),
        BaseRef::Head
    );
    assert_eq!(
        configured_base(&settings(
            serde_json::json!({"worktree": {"baseRef": "odd"}})
        )),
        BaseRef::Fresh
    );
}

#[test]
fn an_existing_linked_worktree_opens_by_path_but_the_main_checkout_does_not() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    let made = create(&request(&repo, "by-path")).unwrap();

    let opened = open_existing(&repo, &made.path).unwrap();
    assert!(opened.existed);
    assert_eq!(opened.path, made.path);
    assert_eq!(opened.branch.as_deref(), Some("worktree-by-path"));
    assert_eq!(opened.base_commit, made.base_commit);

    assert!(matches!(
        open_existing(&repo, &repo),
        Err(WorktreeError::NotAWorktree { .. })
    ));
    let plain = temp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert!(matches!(
        open_existing(&repo, &plain),
        Err(WorktreeError::NotAWorktree { .. })
    ));
}

/// An isolated child's worktree (`rlm.spawn(isolation="worktree")`): the
/// child works in the directory matching the request's place in the
/// repository, and its worktree goes only when it holds no work.
#[test]
fn a_child_worktree_matches_the_cwd_and_is_removed_only_when_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let repo = fixture_repo(&temp);
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    std::fs::write(repo.join("sub/file.txt"), "one\n").unwrap();
    run_fixture_git(&repo, &["add", "sub"]);
    run_fixture_git(&repo, &["commit", "-q", "-m", "sub"]);

    let clean = create(&request(&repo.join("sub"), "agent-x1")).unwrap();
    assert_eq!(
        (
            matching_cwd(&clean, &repo.join("sub")),
            matching_cwd(&clean, &repo.join("missing")),
            matching_cwd(&clean, &repo),
        ),
        (
            clean.path.join("sub"),
            clean.path.clone(),
            clean.path.clone()
        )
    );
    assert!(remove_if_unchanged(&clean).unwrap());
    assert!(!clean.path.exists());
    assert_eq!(
        run_fixture_git(&repo, &["branch", "--list", "worktree-agent-x1"]),
        ""
    );

    let dirty = create(&request(&repo, "agent-x2")).unwrap();
    std::fs::write(dirty.path.join("new.txt"), "work\n").unwrap();
    assert!(!remove_if_unchanged(&dirty).unwrap());
    assert!(dirty.path.join("new.txt").is_file());

    let committed = create(&request(&repo, "agent-x3")).unwrap();
    std::fs::write(committed.path.join("c.txt"), "work\n").unwrap();
    run_fixture_git(&committed.path, &["add", "c.txt"]);
    run_fixture_git(&committed.path, &["commit", "-q", "-m", "work"]);
    assert!(!remove_if_unchanged(&committed).unwrap());
    assert!(committed.path.is_dir());
}
