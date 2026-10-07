//! The venv dir-layout concern: the override-aware
//! kernel venv dir, the writable-dir fallback, and the interpreter path.

use super::store::VenvStore;
use super::{anyhow, Path, PathBuf};

pub(crate) fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

pub(super) fn home_dir() -> PathBuf {
    pa_types::platform::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Directory of the pinned kernel venv (`PRIME_AGENT_KERNEL_VENV`), else
/// `~/.prime/agent/kernel-venv`: the link to the most recently booted keyed
/// venv (the keyed store). Its parent is where the runtime bundles live.
#[must_use]
pub fn kernel_venv_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("PRIME_AGENT_KERNEL_VENV") {
        if !override_dir.is_empty() {
            return expand_home(&override_dir);
        }
    }
    home_dir().join(".prime").join("agent").join("kernel-venv")
}

pub(super) fn xdg_kernel_venv_dir() -> PathBuf {
    let data_home = match std::env::var("XDG_DATA_HOME") {
        Ok(value) if !value.is_empty() => expand_home(&value),
        _ => home_dir().join(".local").join("share"),
    };
    data_home.join("prime").join("agent").join("kernel-venv")
}

/// Where the kernel venv lives: one directory the user pinned
/// (`PRIME_AGENT_KERNEL_VENV`), rebuilt in place when its runtime changes,
/// or the keyed store, one venv per runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KernelVenvLocation {
    Pinned(PathBuf),
    Keyed(VenvStore),
}

/// The agent dirs a store may live under, in order: the managed one
/// (`~/.prime/agent`), then its XDG data fallback.
pub(super) fn store_bases() -> [PathBuf; 2] {
    [
        home_dir().join(".prime").join("agent"),
        xdg_kernel_venv_dir()
            .parent()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf),
    ]
}

/// The venv location for this process: the pinned override when set, else
/// the first store whose directory can be created.
pub(crate) fn resolve_kernel_venv_location() -> anyhow::Result<KernelVenvLocation> {
    if std::env::var("PRIME_AGENT_KERNEL_VENV").is_ok_and(|v| !v.is_empty()) {
        let pinned = kernel_venv_dir();
        if std::fs::create_dir_all(pinned.parent().unwrap_or(Path::new("/"))).is_err() {
            return Err(anyhow!(
                "couldn't create kernel venv parent directories for {}",
                pinned.display()
            ));
        }
        return Ok(KernelVenvLocation::Pinned(pinned));
    }
    let [primary, fallback] = store_bases();
    for base in [&primary, &fallback] {
        let store = VenvStore::new(base.clone());
        if std::fs::create_dir_all(store.root()).is_ok() {
            return Ok(KernelVenvLocation::Keyed(store));
        }
    }
    Err(anyhow!(
        "couldn't create kernel venv directory at {} or {}; set PRIME_AGENT_KERNEL_PYTHON to a python with a current prime-agent-runtime installed",
        VenvStore::new(primary).root().display(),
        VenvStore::new(fallback).root().display()
    ))
}

/// Path of the venv's python interpreter.
#[must_use]
pub fn kernel_venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}
