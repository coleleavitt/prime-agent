//! Shared fixtures for the toolforge integration tests.

#![allow(dead_code)] // each test binary uses its own subset

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_core::kernel::bootstrap::PythonSkillPackageInstallResult;
use pa_toolforge::{PackageInstaller, PublishOptions, PublishRequest};

pub const SLUGIFY_SOURCE: &str = "\"\"\"Turn a string into a URL slug.\"\"\"

import re


def run(text: str) -> str:
    \"\"\"Lowercase text and join its word characters with hyphens.\"\"\"
    return re.sub(r\"[^a-z0-9]+\", \"-\", str(text).lower()).strip(\"-\")
";

pub const SLUGIFY_DOC: &str = "Turn arbitrary text into a lowercase hyphenated slug.";

pub const SLUGIFY_EXIT_TEST: &str = "import slugify

assert slugify.run(\"A B\") == \"a-b\", slugify.run(\"A B\")
assert slugify.run(\"Hello,  World!\") == \"hello-world\", slugify.run(\"Hello,  World!\")
";

/// A Python 3 for the gate runs: `PA_TOOLFORGE_PYTHON`, else the kernel venv
/// under the test home (never the real one), else `python3` on `PATH`. `None` skips the test with a note.
pub fn gate_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_TOOLFORGE_PYTHON") {
        return Some(PathBuf::from(explicit));
    }
    let kernel =
        pa_types::platform::test_isolation::test_home().join(".prime/agent/kernel-venv/bin/python");
    if kernel.exists() {
        return Some(kernel);
    }
    let found = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
    });
    if found.is_none() {
        eprintln!("no python3 found; skipping the toolforge gate test");
    }
    found
}

pub fn slugify_request() -> PublishRequest {
    PublishRequest {
        name: "slugify".to_string(),
        source: SLUGIFY_SOURCE.to_string(),
        doc: SLUGIFY_DOC.to_string(),
        exit_test: SLUGIFY_EXIT_TEST.to_string(),
    }
}

/// Promotes exactly like the real installer but never touches a kernel venv;
/// records each promoted package path.
pub fn recording_installer(installs: &Arc<Mutex<Vec<PathBuf>>>) -> PackageInstaller {
    let installs = Arc::clone(installs);
    Arc::new(move |request, promote| {
        let installs = Arc::clone(&installs);
        Box::pin(async move {
            promote()?;
            installs.lock().unwrap().push(request.package_path);
            Ok(PythonSkillPackageInstallResult {
                installed: false,
                detail: "test installer: promoted without installing".to_string(),
                duration_ms: 0,
                python: None,
            })
        })
    })
}

/// Options writing under `agent_dir`, gating with `python`, installing with
/// the recording installer.
pub fn options(
    agent_dir: &Path,
    python: &Path,
    installs: &Arc<Mutex<Vec<PathBuf>>>,
) -> PublishOptions {
    let mut options = PublishOptions::for_agent_dir(agent_dir);
    options.python = Some(python.to_path_buf());
    options.installer = recording_installer(installs);
    options.session_id = Some("toolforge-test".to_string());
    options
}
