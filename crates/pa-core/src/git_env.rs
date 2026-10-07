//! The git environment that selects a repository.
//!
//! `GIT_DIR`, `GIT_WORK_TREE` and their siblings override repository discovery: a `git`
//! child that inherits them operates on the repository they name, whatever its
//! `current_dir`. Git exports them to every hook and `rebase --exec` command, so a process
//! started from one inherits the outer repository's.
//!
//! Two postures, chosen per call site:
//!
//! - A git call about a directory the caller owns or names (a package checkout, a workspace
//!   snapshot of `cwd`) scrubs them with [`scrub_repository_selection`], so it always sees that
//!   directory's own repository.
//! - A git call made on the user's behalf in the user's shell environment (the bash tool and its
//!   guards' probes, the session header's git context) keeps them: an exported `GIT_DIR` is the
//!   user's explicit choice, and the probe must see the repository the guarded command will.
//!
//! [`isolate_fixture`] goes further for test fixtures: no ambient repository, no user or system
//! config, discovery bounded at a ceiling, and a fixed identity.

use std::path::Path;

/// Variables that select the repository, index, object store, or ref namespace a git
/// command operates on, or reinterpret its pathspecs relative to another directory.
pub const REPOSITORY_SELECTION_ENV: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// Variables that inject configuration into a git command (`GIT_CONFIG_PARAMETERS` carries
/// `git -c` values into children; `GIT_CONFIG_COUNT` and its `GIT_CONFIG_KEY_<n>` /
/// `GIT_CONFIG_VALUE_<n>` pairs are the env form of the same thing).
const INJECTED_CONFIG_ENV: [&str; 3] = ["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT", "GIT_CONFIG"];

/// The fixed identity [`isolate_fixture`] gives fixture commits (a throwaway temp repo's, never this
/// repository's history).
pub const FIXTURE_IDENTITY_ENV: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Prime Agent Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.invalid"),
    ("GIT_COMMITTER_NAME", "Prime Agent Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.invalid"),
];

/// Remove every [`REPOSITORY_SELECTION_ENV`] variable from `command`'s environment, so the
/// child discovers its repository from its `current_dir`.
pub fn scrub_repository_selection(
    command: &mut std::process::Command,
) -> &mut std::process::Command {
    for name in REPOSITORY_SELECTION_ENV {
        command.env_remove(name);
    }
    command
}

/// A git command for a test fixture rooted at `cwd`: [`scrub_repository_selection`], no
/// injected, global, or system config (`GIT_CONFIG_GLOBAL=/dev/null`,
/// `GIT_CONFIG_NOSYSTEM=1`), discovery that never climbs to or above `ceiling`
/// (`GIT_CEILING_DIRECTORIES`), a fixed author and committer, and `TZ=UTC`.
///
/// Fixtures pass their temp root's parent as `ceiling`: a fixture command that runs before its
/// `git init` (or whose init failed) then fails instead of finding an enclosing checkout.
pub fn isolate_fixture<'a>(
    command: &'a mut std::process::Command,
    ceiling: &Path,
) -> &'a mut std::process::Command {
    scrub_repository_selection(command);
    for name in INJECTED_CONFIG_ENV {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name.starts_with("GIT_CONFIG_KEY_") || name.starts_with("GIT_CONFIG_VALUE_") {
            command.env_remove(name.as_ref());
        }
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CEILING_DIRECTORIES", ceiling)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("TZ", "UTC")
        .envs(FIXTURE_IDENTITY_ENV)
}

/// `git` in `cwd`, [`isolate_fixture`]d with the temp root as the discovery ceiling: the one
/// constructor every test fixture in the workspace runs git through.
#[must_use]
pub fn fixture_git(cwd: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command.current_dir(cwd);
    isolate_fixture(&mut command, &std::env::temp_dir());
    command
}

/// Run [`fixture_git`] with `args` and return its trimmed stdout.
///
/// # Panics
///
/// When git cannot start or exits non-zero: a fixture that cannot build its repository has
/// nothing to test.
#[expect(
    clippy::must_use_candidate,
    reason = "most fixture steps run git for its effect; the stdout is for the few that read it"
)]
pub fn run_fixture_git(cwd: &Path, args: &[&str]) -> String {
    let output = fixture_git(cwd)
        .args(args)
        .output()
        .expect("git is required for this fixture");
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `env` with every [`REPOSITORY_SELECTION_ENV`] variable removed: for children whose
/// environment is an explicit map (the bash tool's spawn hook, a guard context).
pub fn without_repository_selection<M>(mut env: M) -> M
where
    M: RepositorySelectionMap,
{
    for name in REPOSITORY_SELECTION_ENV {
        env.remove_name(name);
    }
    env
}

/// An environment map [`without_repository_selection`] can scrub.
pub trait RepositorySelectionMap {
    /// Remove `name`.
    fn remove_name(&mut self, name: &str);
}

impl<S: std::hash::BuildHasher> RepositorySelectionMap
    for std::collections::HashMap<String, String, S>
{
    fn remove_name(&mut self, name: &str) {
        self.remove(name);
    }
}

impl RepositorySelectionMap for std::collections::BTreeMap<String, String> {
    fn remove_name(&mut self, name: &str) {
        self.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn removed(command: &std::process::Command) -> Vec<String> {
        let mut names: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn scrub_removes_exactly_the_selection_variables() {
        let mut command = std::process::Command::new("git");
        scrub_repository_selection(&mut command);
        let mut expected: Vec<String> = REPOSITORY_SELECTION_ENV
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        expected.sort();
        assert_eq!(removed(&command), expected);
    }

    #[test]
    fn a_later_explicit_selection_still_wins_over_the_scrub() {
        let mut command = std::process::Command::new("git");
        scrub_repository_selection(&mut command).env("GIT_DIR", "/explicit");
        let git_dir = command
            .get_envs()
            .find(|(name, _)| *name == "GIT_DIR")
            .map(|(_, value)| value.map(std::ffi::OsStr::to_os_string));
        assert_eq!(git_dir, Some(Some("/explicit".into())));
    }

    #[test]
    fn maps_lose_the_selection_variables_and_keep_the_rest() {
        let env: std::collections::HashMap<String, String> = [
            ("GIT_DIR", "/elsewhere/.git"),
            ("GIT_WORK_TREE", "/elsewhere"),
            ("GIT_EDITOR", "true"),
            ("PATH", "/usr/bin"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
        let expected: std::collections::HashMap<String, String> =
            [("GIT_EDITOR", "true"), ("PATH", "/usr/bin")]
                .into_iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect();
        assert_eq!(without_repository_selection(env), expected);
    }

    /// Set in the re-executed child so the sentinel test does not recurse.
    #[cfg(unix)]
    const SENTINEL_CHILD_ENV: &str = "PA_CORE_GIT_SENTINEL_CHILD";

    /// The bytes that would show a git command reaching the sentinel: `HEAD`, `config` (a
    /// `core.bare` flip, an added `[user]` section), the index, and every ref.
    #[cfg(unix)]
    fn sentinel_state(git_dir: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
        fn walk(dir: &Path, root: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    let name = path.strip_prefix(root).unwrap().display().to_string();
                    out.insert(name, std::fs::read(&path).unwrap());
                }
            }
        }
        let mut state = std::collections::BTreeMap::new();
        for name in ["HEAD", "config", "index", "packed-refs"] {
            if let Ok(bytes) = std::fs::read(git_dir.join(name)) {
                state.insert(name.to_string(), bytes);
            }
        }
        walk(&git_dir.join("refs"), git_dir, &mut state);
        state
    }

    /// Re-run `tests` (exact names) in a child of this test binary whose environment exports
    /// `GIT_DIR`/`GIT_WORK_TREE` at a sentinel repository, as a git hook or `rebase --exec`
    /// would; return the child's output and whether the sentinel kept every byte.
    #[cfg(unix)]
    fn run_under_sentinel(tests: &[&str]) -> (std::process::Output, bool) {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("sentinel");
        std::fs::create_dir(&repo).unwrap();
        run_fixture_git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("seed.txt"), "seed\n").unwrap();
        run_fixture_git(&repo, &["add", "seed.txt"]);
        run_fixture_git(&repo, &["commit", "-q", "-m", "seed"]);
        let git_dir = repo.join(".git");
        let before = sentinel_state(&git_dir);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(tests)
            .args(["--exact", "--test-threads", "4"])
            .env("GIT_DIR", &git_dir)
            .env("GIT_WORK_TREE", &repo)
            .env(SENTINEL_CHILD_ENV, "1")
            .output()
            .unwrap();
        let unchanged = sentinel_state(&git_dir) == before;
        (output, unchanged)
    }

    /// The incident class: tests run from `git rebase --exec` inherited the outer repository's
    /// `GIT_DIR`, and their fixture git re-initialized it bare, added a `[user]` section, and
    /// made branches and commits in it. Under an exported `GIT_DIR` the git-running tests must
    /// leave that repository byte-identical and still pass.
    #[cfg(unix)]
    #[test]
    fn git_tests_leave_an_inherited_repository_untouched() {
        if std::env::var_os(SENTINEL_CHILD_ENV).is_some() {
            return;
        }
        let (output, unchanged) = run_under_sentinel(&[
            "packages::tests::git_clone_update_remove_flow",
            "packages::tests::git_ref_checkout_installs_the_pinned_revision",
            "autonomous::gates::tests::shell_runner_runs_gates_and_snapshots",
            "autonomous::gates::tests::shell_runner_snapshot_tracks_workspace_changes",
            "tools::golden_replay::golden_bash_group_matches_ts",
            "workspace_snapshot::tests::snapshot_captures_unmerged_conflict_worktree_content",
        ]);
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            unchanged,
            "a test wrote to the inherited repository:\n{log}"
        );
        assert!(output.status.success(), "{log}");
        assert!(log.contains("test result: ok. 6 passed"), "{log}");
    }

    /// The session header's git context honours an exported `GIT_DIR` on purpose (the user's
    /// choice), so this test's assertions read the sentinel; its fixture writes must still not.
    #[cfg(unix)]
    #[test]
    fn session_git_state_tests_never_write_an_inherited_repository() {
        if std::env::var_os(SENTINEL_CHILD_ENV).is_some() {
            return;
        }
        let (output, unchanged) =
            run_under_sentinel(&["session_engine::tests::run_boundaries_record_git_state"]);
        assert!(
            unchanged,
            "a test wrote to the inherited repository:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
