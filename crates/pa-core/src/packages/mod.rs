//! Package manager subsystem: install/remove/list/update of `npm:`, git,
//! and local-dir package sources against the settings store, plus the
//! configured-npm/git child-process flows and session resource resolution.
//! Non-goals: loading/executing session-resource code and self-updates.

mod git;
mod manager;
mod npm;
mod process;
pub(crate) mod resolve;
pub mod resource_config;
mod source;
mod update;

#[cfg(test)]
mod tests;

pub use manager::{
    BundledSkillsDir, ConfiguredPackage, PackageManager, PackageManagerOptions, PackageUpdate,
    ProgressAction, ProgressEvent, ProgressEventKind, UserOrProject,
};
pub use resolve::{
    MetadataSource, MissingSourceAction, PathMetadata, ResolvedPaths, ResolvedResource,
    ResourceOrigin, ResourceType,
};
pub(crate) use source::is_local_path;
pub use source::{parse_git_url, GitSource, LocalSource, NpmSource, ParsedSource, SourceScope};

use std::fmt::Write as _;
use std::path::PathBuf;

/// The TS `CONFIG_DIR_NAME` (project-local settings/packages root).
pub use crate::settings::CONFIG_DIR_NAME;

/// Network probe timeout for npm/git operations (10s).
pub(crate) use npm::NETWORK_TIMEOUT_MS;

/// True when `PI_OFFLINE` disables all package network operations.
pub(crate) fn is_offline_mode_enabled() -> bool {
    std::env::var("PI_OFFLINE").is_ok_and(|value| {
        value == "1" || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes")
    })
}

/// The package directory: `PI_PACKAGE_DIR` wins (matching the TS
/// `getPackageDir` override), then the directory of the executable (the
/// packaged bun-binary layout).
pub(crate) fn package_dir() -> PathBuf {
    if let Ok(env_dir) = std::env::var("PI_PACKAGE_DIR") {
        if !env_dir.is_empty() {
            return expand_tilde(&env_dir);
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The bundled docs directory (TS `getDocsPath`): `<package dir>/docs`.
#[must_use]
pub fn docs_path() -> PathBuf {
    package_dir().join("docs")
}

fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

fn home_dir() -> PathBuf {
    pa_types::platform::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// The subdirectory of the bundled skills directory that holds the skills
/// installed features contribute. Its leading dot hides it from every native
/// skill scan, so a build without the feature never sees them; it ships with
/// `skills/` and needs no packaging of its own.
pub const FEATURE_SKILLS_DIR: &str = ".features";

/// The directory of built-in skills shipped with the package (TS
/// `getBundledSkillsDir`): `skills/` in the package dir (the packaged
/// layout, or an explicit `PI_PACKAGE_DIR`), else the skills embedded in this
/// binary, extracted beside the kernel venv. The live source checkout is never
/// used: its skills may need a host newer or older than this binary.
pub(crate) fn get_bundled_skills_dir() -> PathBuf {
    let packaged = package_dir().join("skills");
    if packaged.is_dir() || std::env::var_os("PI_PACKAGE_DIR").is_some_and(|dir| !dir.is_empty()) {
        return packaged;
    }
    crate::embedded_bundle::embedded_bundle_dir()
        .map(|bundle| bundle.join(crate::embedded_bundle::SKILLS_DIR))
        .filter(|skills| skills.is_dir())
        .unwrap_or(packaged)
}

/// Stable temporary directory for resolve-only package installs (the hash
/// keys on prefix+suffix so the same source always maps to one checkout).
pub(crate) fn temporary_dir(prefix: &str, suffix: Option<&str>) -> PathBuf {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(format!("{prefix}-{}", suffix.unwrap_or_default()).as_bytes());
    let digest = hasher.finalize();
    let hash: String = digest[..4].iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    });
    std::env::temp_dir()
        .join("pi-extensions")
        .join(prefix)
        .join(&hash)
        .join(suffix.unwrap_or_default())
}

#[cfg(test)]
pub(crate) mod test_support {
    /// Process-wide env reads and writes (HOME, `PI_OFFLINE`) serialize
    /// through one lock across the packages test modules: parallel test
    /// threads in the same binary otherwise race the process env.
    pub(crate) static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take [`ENV_MUTEX`], ignoring poisoning: the guarded data is `()`, so a
    /// test that panicked while holding it must not fail every later test.
    pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
