//! Per-OS daemon endpoint naming: Unix socket files under
//! `<tmpdir>/prime-agent-<uid>/`; Windows named pipes in the `\\.\\pipe\\`
//! namespace (a per-user daemon pipe, hashed worker pipes).
use std::path::{Path, PathBuf};

use crate::paths::hash_key;

/// Default directory holding daemon socket files (Unix): `$XDG_RUNTIME_DIR/prime-agent` when
/// the runtime dir is a private directory of this user, else [`legacy_socket_dir`].
///
/// `/tmp` is swept by tmpfiles cleaners, tmp wipes and test harnesses; every socket, the
/// supervisor's lock directory and each worker's endpoint go with it while the processes live
/// on, unreachable. The runtime dir is per-user, mode 0700 and cleared only at logout.
#[cfg(unix)]
pub fn socket_dir() -> PathBuf {
    let uid = pa_core::platform::process::current_user_id();
    runtime_socket_dir(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from), uid)
        .unwrap_or_else(legacy_socket_dir)
}

/// The directory daemons used before the runtime dir was preferred: `<tmpdir>/prime-agent-<uid>`
/// (the TypeScript daemon's). Discovery still accepts daemons sitting there.
#[cfg(unix)]
pub fn legacy_socket_dir() -> PathBuf {
    let uid = pa_core::platform::process::current_user_id();
    let tmp = std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    socket_dir_for(&tmp, uid)
}

/// `<runtime>/prime-agent` for a runtime dir that is absolute, a real directory (no symlink)
/// owned by `uid`, and closed to group and others. Anything else would let another user place
/// or replace the daemon's sockets, so it is not used.
#[cfg(unix)]
fn runtime_socket_dir(runtime: Option<PathBuf>, uid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let runtime = runtime.filter(|dir| dir.is_absolute())?;
    let metadata = std::fs::symlink_metadata(&runtime).ok()?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return None;
    }
    let dir = runtime.join("prime-agent");
    #[cfg(not(target_os = "linux"))]
    if std::os::unix::net::SocketAddr::from_pathname(
        dir.join("worker-000000000000-000000000000.sock"),
    )
    .is_err()
    {
        return None;
    }
    Some(dir)
}

/// Reserve the longest generated endpoint before selecting a directory. Linux
/// can re-anchor deep paths through /proc; other Unix platforms need a short path.
#[cfg(unix)]
fn socket_dir_for(tmp: &Path, uid: u32) -> PathBuf {
    let name = format!("prime-agent-{uid}");
    let dir = tmp.join(&name);
    #[cfg(not(target_os = "linux"))]
    if std::os::unix::net::SocketAddr::from_pathname(
        dir.join("worker-000000000000-000000000000.sock"),
    )
    .is_err()
    {
        return Path::new("/tmp").join(name);
    }
    dir
}

/// The socket-dir half of a discovery state root on Windows: TS computes
/// `<tmpdir>/prime-agent-user` so `DaemonStateRoot` keeps one shape.
#[cfg(not(unix))]
#[must_use]
pub fn socket_dir() -> PathBuf {
    std::env::temp_dir().join("prime-agent-user")
}

/// Off unix there is one socket dir: no earlier location to keep honoring.
#[cfg(not(unix))]
#[must_use]
pub fn legacy_socket_dir() -> PathBuf {
    socket_dir()
}

/// Default supervisor endpoint: `daemon.sock` in the socket dir (Unix) or
/// the per-user daemon pipe name (Windows), keyed by the agent dir when it is
/// not the product default (see [`agent_dir_socket_suffix`]).
///
/// A daemon still running from the legacy socket dir (started by an earlier build) keeps being
/// the default until it stops: resolving to the new dir meanwhile would start a second daemon
/// and hide the first one's sessions from every new client.
#[cfg(unix)]
#[must_use]
pub fn default_daemon_socket_path() -> PathBuf {
    default_socket_in(&socket_dir(), &legacy_socket_dir())
}

#[cfg(unix)]
fn default_socket_in(current: &Path, legacy: &Path) -> PathBuf {
    let name = match current_agent_dir_socket_suffix() {
        Some(suffix) => format!("daemon-{suffix}.sock"),
        None => "daemon.sock".to_string(),
    };
    let legacy_socket = legacy.join(&name);
    if legacy != current && crate::socket::lease_is_live(&legacy_socket) {
        legacy_socket
    } else {
        current.join(name)
    }
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

    #[cfg(unix)]
    #[test]
    fn the_legacy_socket_dir_uses_the_same_real_uid_as_the_typescript_daemon() {
        let output = std::process::Command::new("id")
            .arg("-ru")
            .output()
            .unwrap();
        assert!(output.status.success());
        let uid = String::from_utf8(output.stdout).unwrap();
        let tmp = std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        assert_eq!(
            legacy_socket_dir(),
            socket_dir_for(&tmp, uid.trim().parse().unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_default_socket_follows_a_live_legacy_daemon_and_otherwise_the_current_dir() {
        let scratch = tempfile::TempDir::new().unwrap();
        let current = scratch.path().join("run/prime-agent");
        let legacy = scratch.path().join("tmp/prime-agent-1000");
        std::fs::create_dir_all(&legacy).unwrap();
        let name = default_socket_in(&current, &current)
            .file_name()
            .unwrap()
            .to_owned();
        let legacy_socket = legacy.join(&name);

        assert_eq!(default_socket_in(&current, &legacy), current.join(&name));

        let _listener = std::os::unix::net::UnixListener::bind(&legacy_socket).unwrap();
        assert_eq!(
            default_socket_in(&current, &legacy),
            current.join(&name),
            "a socket file nobody holds the lease on is a leftover"
        );

        let lock = pa_core::platform::LockDir::path_for(&legacy_socket);
        std::fs::create_dir(&lock).unwrap();
        assert_eq!(default_socket_in(&current, &legacy), legacy_socket);

        std::fs::File::open(&lock)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            default_socket_in(&current, &legacy),
            current.join(&name),
            "a lease nobody refreshes is a dead daemon's"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_private_runtime_dir_of_this_user_hosts_the_sockets() {
        use std::os::unix::fs::PermissionsExt;

        let uid = pa_core::platform::process::current_user_id();
        let runtime = tempfile::TempDir::new().unwrap();
        std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            runtime_socket_dir(Some(runtime.path().to_path_buf()), uid),
            Some(runtime.path().join("prime-agent"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_runtime_dir_that_is_open_foreign_relative_or_a_symlink_is_not_used() {
        use std::os::unix::fs::PermissionsExt;

        let uid = pa_core::platform::process::current_user_id();
        let scratch = tempfile::TempDir::new().unwrap();
        let open = scratch.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        let private = scratch.path().join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = scratch.path().join("link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        for unusable in [
            (Some(open), uid),
            (Some(private), uid.wrapping_add(1)),
            (Some(PathBuf::from("run/user")), uid),
            (Some(link), uid),
            (Some(scratch.path().join("missing")), uid),
            (None, uid),
        ] {
            assert_eq!(runtime_socket_dir(unusable.0, unusable.1), None);
        }
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn generated_endpoints_fit_even_when_tmpdir_is_deep() {
        use std::os::unix::net::SocketAddr;

        for length in [20, 65, 120] {
            let tmp = PathBuf::from(format!("/{}", "d".repeat(length - 1)));
            let dir = socket_dir_for(&tmp, 501);
            let worker = dir.join("worker-000000000000-000000000000.sock");
            assert!(SocketAddr::from_pathname(&worker).is_ok());
            if length >= 65 {
                assert_eq!(dir, Path::new("/tmp/prime-agent-501"));
            } else {
                assert_eq!(dir, tmp.join("prime-agent-501"));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn short_tmpdir_keeps_the_original_socket_directory() {
        assert_eq!(
            socket_dir_for(Path::new("/tmp"), 501),
            Path::new("/tmp/prime-agent-501")
        );
    }

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
