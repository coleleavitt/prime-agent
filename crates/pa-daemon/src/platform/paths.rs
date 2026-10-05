//! Per-OS daemon endpoint naming: Unix socket files under
//! `<tmpdir>/prime-agent-<uid>/`; Windows named pipes in the `\\.\\pipe\\`
//! namespace (a per-user daemon pipe, hashed worker pipes).
use std::path::{Path, PathBuf};

use crate::paths::hash_key;

/// Default directory holding daemon socket files (Unix).
#[cfg(unix)]
pub fn socket_dir() -> PathBuf {
    let uid = current_uid().unwrap_or_else(|| "user".to_string());
    let tmp = std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    tmp.join(format!("prime-agent-{uid}"))
}

/// The socket-dir half of a discovery state root on Windows: TS computes
/// `<tmpdir>/prime-agent-user` so `DaemonStateRoot` keeps one shape.
#[cfg(not(unix))]
#[must_use]
pub fn socket_dir() -> PathBuf {
    std::env::temp_dir().join("prime-agent-user")
}

/// Read the effective uid without libc: `/proc/self/status` on Linux,
/// HOME-derived uniqueness elsewhere (best-effort, same as today).
#[cfg(unix)]
fn current_uid() -> Option<String> {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                if let Some(first) = rest.split_whitespace().next() {
                    return Some(first.to_string());
                }
            }
        }
    }
    None
}

/// Default supervisor endpoint: `daemon.sock` in the socket dir (Unix) or
/// the per-user daemon pipe name (Windows), keyed by the agent dir when it is
/// not the product default (see [`agent_dir_socket_suffix`]).
#[cfg(unix)]
#[must_use]
pub fn default_daemon_socket_path() -> PathBuf {
    socket_dir().join(match current_agent_dir_socket_suffix() {
        Some(suffix) => format!("daemon-{suffix}.sock"),
        None => "daemon.sock".to_string(),
    })
}

#[cfg(not(unix))]
#[must_use]
pub fn default_daemon_socket_path() -> PathBuf {
    // The SID cannot be spoofed through the environment; USERNAME is the fallback if the token
    // query ever fails. The name only separates users - the pipe's owner-only ACL and the
    // client's owner check are what keep other accounts out.
    let user = pa_types::platform::current_user_sid()
        .unwrap_or_else(|_| std::env::var("USERNAME").unwrap_or_default());
    let pipe = per_user_daemon_pipe_path(&user);
    match current_agent_dir_socket_suffix() {
        Some(suffix) => PathBuf::from(format!("{}-{suffix}", pipe.display())),
        None => pipe,
    }
}

/// The Windows daemon pipe for one user: a fixed machine-global name let every account on the
/// machine reach (or squat) the same daemon endpoint.
#[cfg(any(windows, test))]
fn per_user_daemon_pipe_path(user: &str) -> PathBuf {
    PathBuf::from(format!(
        r"\\.\pipe\prime-agent-daemon-{}",
        hash_key(user, 12)
    ))
}

/// [`agent_dir_socket_suffix`] for this process's agent dir; an unresolvable
/// agent dir keeps the unkeyed default.
fn current_agent_dir_socket_suffix() -> Option<String> {
    let agent_dir = crate::paths::agent_dir().ok()?;
    let default_agent_dir = crate::paths::home_dir()
        .ok()
        .map(|home| home.join(crate::paths::CONFIG_DIR_NAME));
    agent_dir_socket_suffix(&agent_dir, default_agent_dir.as_deref())
}

/// The daemon identity includes the agent state dir (upstream #768/#786/#815):
/// two installs sharing the uid-keyed socket (a `PRIME_AGENT_CODING_AGENT_DIR`
/// install beside the default one) would otherwise attach to whichever daemon
/// started first and run their sessions under its state. The product-default
/// agent dir keeps the unkeyed endpoint, so existing daemons stay reachable;
/// any other dir gets an 8-char hash of its absolute path. The hash goes into
/// the socket file name, not the directory, so worker socket paths (which
/// live in the shared socket dir) do not grow.
#[must_use]
pub fn agent_dir_socket_suffix(
    agent_dir: &Path,
    default_agent_dir: Option<&Path>,
) -> Option<String> {
    let absolute = |path: &Path| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let agent_dir = absolute(agent_dir);
    if default_agent_dir.is_some_and(|default| absolute(default) == agent_dir) {
        return None;
    }
    Some(hash_key(&agent_dir.to_string_lossy(), 8))
}

/// Worker endpoint next to the supervisor's: hashed supervisor key plus the
/// worker id prefix (TS `workerSocketPath`).
#[cfg(unix)]
#[must_use]
pub fn worker_socket_path(supervisor_socket_path: &Path, worker_id: &str) -> PathBuf {
    let key = hash_key(&supervisor_socket_path.to_string_lossy(), 12);
    socket_dir().join(format!(
        "worker-{key}-{}.sock",
        &worker_id[..12.min(worker_id.len())]
    ))
}

#[cfg(not(unix))]
#[must_use]
pub fn worker_socket_path(supervisor_socket_path: &Path, worker_id: &str) -> PathBuf {
    let key = hash_key(&supervisor_socket_path.to_string_lossy(), 12);
    PathBuf::from(format!(
        r"\\.\pipe\prime-agent-worker-{key}-{}",
        &worker_id[..12.min(worker_id.len())]
    ))
}

// Socket-filesystem identity is the shared platform contract
// `pa_types::platform::socket_identity`: the same helper serves stale-file
// cleanup here and direct-transport ticket validation in the clients.

pub use pa_types::daemon::SocketIdentity;
pub use pa_types::platform::socket_identity;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_socket_names_are_deterministic() {
        let supervisor = Path::new("/tmp/prime-agent-1/daemon.sock");
        let a = worker_socket_path(supervisor, "0123456789abcdef");
        let b = worker_socket_path(supervisor, "fedcba9876543210");
        assert_ne!(a, b);
        // Only the first 12 id characters key the name.
        assert_eq!(a, worker_socket_path(supervisor, "0123456789abffff"));
        #[cfg(unix)]
        assert!(a.starts_with(socket_dir()));
    }

    /// Upstream #2785: the Windows daemon pipe is per-user (a hash of the user's SID), never the
    /// machine-global fixed name every account shared.
    #[test]
    fn the_windows_daemon_pipe_is_per_user() {
        let alice = per_user_daemon_pipe_path("S-1-5-21-1-2-3-1001");
        let bob = per_user_daemon_pipe_path("S-1-5-21-1-2-3-1002");
        assert_eq!(
            (alice.to_string_lossy().into_owned(), alice == bob,),
            (
                format!(
                    r"\\.\pipe\prime-agent-daemon-{}",
                    hash_key("S-1-5-21-1-2-3-1001", 12)
                ),
                false,
            )
        );
    }

    /// The Windows endpoint names: this user's daemon pipe and the hashed worker pipe name in
    /// the `\\.\\pipe\\` namespace.
    #[test]
    #[cfg(windows)]
    fn windows_endpoints_are_the_pipe_names() {
        assert_eq!(
            default_daemon_socket_path(),
            per_user_daemon_pipe_path(&pa_types::platform::current_user_sid().expect("user sid"))
        );
        let supervisor = Path::new(r"\\.\pipe\prime-agent-daemon");
        let a = worker_socket_path(supervisor, "0123456789abcdef");
        let rendered = a.to_string_lossy();
        assert!(
            rendered.starts_with(r"\\.\pipe\prime-agent-worker-"),
            "the worker pipe namespace: {rendered}"
        );
        assert!(
            rendered.ends_with("-0123456789ab"),
            "the 12-char id suffix: {rendered}"
        );
    }
}
