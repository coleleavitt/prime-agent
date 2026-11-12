//! Test-only fixtures shared across the crate's unit tests.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

thread_local! {
    /// The scratch dirs [`ThreadTempDir`] handed out on this thread.
    static THREAD_TEMP_DIRS: RefCell<Vec<tempfile::TempDir>> = const { RefCell::new(Vec::new()) };
}

/// A scratch dir that lives until the current thread exits: libtest runs every test on its own
/// thread, so the dir goes when the test ends (after its body and locals, panics included). For
/// fixture helpers that hand back a value living in the dir (a session manager, an engine) but
/// not the dir itself: a plain guard local to the helper would delete the dir on return, and
/// the value's later writes would recreate it outside any guard.
pub(crate) struct ThreadTempDir(PathBuf);

impl ThreadTempDir {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().expect("create a test temp dir");
        let path = dir.path().to_path_buf();
        THREAD_TEMP_DIRS.with(|dirs| dirs.borrow_mut().push(dir));
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

/// Every unit test that runs git builds its repository through [`crate::git_env::fixture_git`]:
/// no inherited repository selection (`GIT_DIR` and its siblings, which git hooks and
/// `rebase --exec` export), no user or system config, discovery stopped at the temp root, and a
/// fixed identity.
pub(crate) use crate::git_env::run_fixture_git as run_git;

/// Set in the child [`run_test_alone`] starts.
const ALONE_CHILD_ENV: &str = "PA_CORE_TEST_ALONE_CHILD";

/// True in the child [`run_test_alone`] starts: the test runs its body there.
pub(crate) fn in_alone_child() -> bool {
    std::env::var_os(ALONE_CHILD_ENV).is_some()
}

/// Run the test `name` (its full path) alone in a child of this test binary, with `env` applied
/// (`None` removes a variable), and assert it passed. For a test that changes the process env
/// (`HOME`, the runtime-source variables): every test in this binary shares that env, and a
/// change here alters what concurrent tests resolve. The child runs only this test, so its body
/// may set the env itself.
pub(crate) fn run_test_alone(name: &str, env: &[(&str, Option<&Path>)]) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", name, "--test-threads=1"])
        .env(ALONE_CHILD_ENV, "1");
    for (key, value) in env {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    let output = command.output().unwrap();
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{log}");
    assert!(log.contains("test result: ok. 1 passed"), "{log}");
}
