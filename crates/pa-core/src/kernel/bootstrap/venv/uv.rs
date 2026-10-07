//! The uv discovery concern: the PATH/PATHEXT executable search and the `ensure_uv` resolution.

use super::{anyhow, home_dir, Path, PathBuf};
#[cfg(windows)]
use crate::platform::process::windows_executable_candidates;

const UV_INSTALL_COMMAND: &str = "curl -LsSf https://astral.sh/uv/install.sh | sh";

fn find_executable(name: &str) -> Option<PathBuf> {
    let path_value = std::env::var("PATH").ok()?;
    // The bare name on Unix; PATHEXT extension candidates on Windows
    // (TS `findExecutable` -> `windowsExecutableCandidates`).
    #[cfg(windows)]
    let candidates = windows_executable_candidates(name, std::env::var("PATHEXT").ok().as_deref());
    #[cfg(not(windows))]
    let candidates = vec![name.to_string()];
    for dir in std::env::split_paths(&path_value) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for candidate in &candidates {
            let full_path = dir.join(candidate);
            if full_path.is_file() && is_executable(&full_path) {
                return Some(full_path);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    crate::platform::perms::is_executable(path)
}

/// Find `uv` on PATH or at `~/.local/bin/uv` (`uv.exe` on Windows). Returns `Err` with install
/// guidance when missing: the Rust binary never auto-installs.
pub(crate) fn ensure_uv() -> anyhow::Result<String> {
    if let Some(from_path) = find_executable("uv") {
        return Ok(from_path.to_string_lossy().to_string());
    }
    let uv_name = if cfg!(windows) { "uv.exe" } else { "uv" };
    let local_uv = home_dir().join(".local").join("bin").join(uv_name);
    if is_executable(&local_uv) {
        return Ok(local_uv.to_string_lossy().to_string());
    }
    Err(anyhow!(
        "uv is required to set up the Python kernel. Install uv yourself: {UV_INSTALL_COMMAND}"
    ))
}
