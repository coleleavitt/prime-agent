//! `prime-agent -w, --worktree [name]` end to end: the launch creates (or
//! reuses) the managed worktree before anything reads the directory, refuses
//! a resume, and fails outside git. `--list-sessions` stands in for a session
//! (it runs in the launch directory and exits without a daemon or a model).
//! Fixture repositories are hermetic: no global or system git config,
//! `TZ=UTC`, a fixed identity.

use std::path::Path;
use std::process::Command;

use pa_core::git_env::run_fixture_git;

fn run(sandbox: &Path, cwd: &Path, args: &[&str]) -> (Option<i32>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_prime-agent"));
    pa_types::platform::test_isolation::TestState::new(sandbox).apply(&mut command);
    command
        .args(args)
        .env("PRIME_AGENT_CODING_AGENT_DIR", sandbox.join("agent"))
        .env("HOME", sandbox)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CEILING_DIRECTORIES", sandbox)
        .env("TZ", "UTC")
        .env("PI_OFFLINE", "1")
        .current_dir(cwd);
    for name in pa_core::git_env::REPOSITORY_SELECTION_ENV {
        command.env_remove(name);
    }
    let output = command.output().expect("spawn prime-agent");
    (
        output.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn fixture_repo(sandbox: &Path) -> std::path::PathBuf {
    let repo = sandbox.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run_fixture_git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    run_fixture_git(&repo, &["add", "README.md"]);
    run_fixture_git(&repo, &["commit", "-q", "-m", "seed"]);
    std::fs::canonicalize(repo).unwrap()
}

#[test]
fn the_flag_creates_then_reuses_the_named_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = std::fs::canonicalize(dir.path()).unwrap();
    let repo = fixture_repo(&sandbox);

    let (code, out) = run(&sandbox, &repo, &["--worktree", "e2e", "--list-sessions"]);
    assert_eq!(code, Some(0), "{out}");
    let worktree = repo.join(".prime/worktrees/e2e");
    assert!(worktree.join("README.md").is_file(), "{out}");
    assert_eq!(
        run_fixture_git(&worktree, &["symbolic-ref", "--short", "HEAD"]),
        "worktree-e2e"
    );
    // The main checkout stays clean: the managed directory is excluded.
    assert_eq!(run_fixture_git(&repo, &["status", "--porcelain"]), "");

    // `-w` again reuses it, from inside the worktree too.
    let (code, out) = run(&sandbox, &worktree, &["-w", "e2e", "--list-sessions"]);
    assert_eq!(code, Some(0), "{out}");
    let listed = run_fixture_git(&repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(listed.matches("worktree ").count(), 2, "{listed}");
}

#[test]
fn the_flag_refuses_a_resume_and_a_directory_outside_git() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = std::fs::canonicalize(dir.path()).unwrap();
    let repo = fixture_repo(&sandbox);

    let (code, out) = run(&sandbox, &repo, &["-w", "x", "--resume", "abc"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains("--worktree cannot be combined with --resume"),
        "{out}"
    );
    assert!(!repo.join(".prime/worktrees/x").exists());

    let plain = sandbox.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let (code, out) = run(&sandbox, &plain, &["--worktree", "--list-sessions"]);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("Error creating worktree"), "{out}");
    assert!(out.contains("not in a git repository"), "{out}");
}
