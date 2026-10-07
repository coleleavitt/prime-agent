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

/// Bash tool options whose children never see an inherited repository selection: the bash
/// tool honours the agent shell's `GIT_DIR` (the user's choice), so tests that run git through
/// it scrub it in the spawn hook.
pub(crate) fn bash_options_without_repository_selection() -> crate::tools::bash::BashToolOptions {
    crate::tools::bash::BashToolOptions {
        spawn_hook: Some(std::sync::Arc::new(|mut context| {
            context.env = crate::git_env::without_repository_selection(context.env);
            context
        })),
        ..Default::default()
    }
}
