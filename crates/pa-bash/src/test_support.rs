//! Test-only fixtures shared across the crate's unit tests.

use std::path::Path;

/// Variables that select the repository a git command operates on. Git exports them to hooks
/// and `rebase --exec` commands, so a test run from one inherits the outer repository's; this
/// crate sits below `pa-core`, so the list mirrors `pa_core::git_env::REPOSITORY_SELECTION_ENV`.
const REPOSITORY_SELECTION_ENV: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// `git` in `cwd` for a fixture repository: no inherited repository selection, no injected,
/// user, or system config, discovery stopped at the temp root, a fixed identity, and an empty
/// `HOME` at `home`. Every unit test that runs git builds its repository through this.
pub(crate) fn git(cwd: &Path, home: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command.current_dir(cwd);
    for name in REPOSITORY_SELECTION_ENV {
        command.env_remove(name);
    }
    for name in ["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT", "GIT_CONFIG"] {
        command.env_remove(name);
    }
    command
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CEILING_DIRECTORIES", std::env::temp_dir())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("TZ", "UTC")
        .env("GIT_AUTHOR_NAME", "Prime Agent Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Prime Agent Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid");
    command
}

/// Run [`git`] with `args` and assert it succeeded.
pub(crate) fn run_git(cwd: &Path, home: &Path, args: &[&str]) {
    let output = git(cwd, home).args(args).output().expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::run_git;

    /// The bytes that would show a git command reaching the sentinel: `HEAD`, `config` (a
    /// `core.bare` flip, an added `[user]` section), the index, and every ref.
    fn sentinel_state(git_dir: &Path) -> BTreeMap<String, Vec<u8>> {
        fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    let name = path
                        .strip_prefix(root)
                        .expect("under root")
                        .display()
                        .to_string();
                    out.insert(name, std::fs::read(&path).expect("read ref"));
                }
            }
        }
        let mut state = BTreeMap::new();
        for name in ["HEAD", "config", "index", "packed-refs"] {
            if let Ok(bytes) = std::fs::read(git_dir.join(name)) {
                state.insert(name.to_string(), bytes);
            }
        }
        walk(&git_dir.join("refs"), git_dir, &mut state);
        state
    }

    /// The incident class: guard tests run from `git rebase --exec` inherited the outer
    /// repository's `GIT_DIR`, and their fixture git added a `[user]` section to it and
    /// re-initialized it bare. Re-run the git-running guard suites in a child of this test
    /// binary with `GIT_DIR`/`GIT_WORK_TREE` exported at a sentinel repository: it keeps every
    /// byte, and the suites still pass.
    #[test]
    fn guard_tests_leave_an_inherited_repository_untouched() {
        let root = tempfile::tempdir().expect("temp dir");
        let repo = root.path().join("sentinel");
        std::fs::create_dir(&repo).expect("sentinel dir");
        run_git(&repo, root.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("seed.txt"), "seed\n").expect("seed");
        run_git(&repo, root.path(), &["add", "seed.txt"]);
        run_git(&repo, root.path(), &["commit", "-q", "-m", "seed"]);
        let git_dir = repo.join(".git");
        let before = sentinel_state(&git_dir);
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "guards::force_push::tests::",
                "guards::destructive_git::tests::",
                "--test-threads",
                "4",
            ])
            .env("GIT_DIR", &git_dir)
            .env("GIT_WORK_TREE", &repo)
            .output()
            .expect("re-run the guard suites");
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(sentinel_state(&git_dir), before, "{log}");
        assert!(output.status.success(), "{log}");
    }
}
