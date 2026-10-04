//! Daemon socket lifecycle: endpoint naming and identity live in
//! [`crate::platform`]; the bind/connect calls go through the shared transport
//! traits in `pa_types::platform`, so Unix socket files today and named pipes later
//! differ only in the implementation module.

use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
use anyhow::anyhow;
use anyhow::Result;

#[cfg(unix)]
pub use crate::platform::socket_dir;
pub use crate::platform::{
    default_daemon_socket_path, socket_identity, worker_socket_path, SocketIdentity,
};

/// Try to connect to an endpoint within `timeout`; true when a peer accepts.
pub async fn can_connect(path: &Path, timeout: Duration) -> bool {
    let connect = pa_types::platform::transport::connect_transport(path);
    match tokio::time::timeout(timeout, connect).await {
        Ok(Ok(stream)) => {
            drop(stream);
            true
        }
        _ => false,
    }
}

/// Staleness after which the cleanup lock of a crashed holder is reclaimed.
/// Unix only: every taker of the cleanup lock sits behind the unix stale-file wall.
#[cfg(unix)]
const LOCK_STALE_AFTER: Duration = Duration::from_secs(5);
/// Live-lock retry cadence and cap (600 retries): ~15s total.
#[cfg(unix)]
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(25);
#[cfg(unix)]
const LOCK_RETRIES: u32 = 600;

/// Acquire the cross-process cleanup lock (proper-lockfile's empty
/// `{path}.lock` directory): held for the whole probe/unlink sequence, so a
/// competing startup worker cannot bind a live listener in between.
#[cfg(unix)]
async fn acquire_cleanup_lock(path: &Path) -> Result<pa_core::platform::LockDir> {
    for attempt in 0..=LOCK_RETRIES {
        match pa_core::platform::LockDir::acquire(path, LOCK_STALE_AFTER) {
            Ok(lock) => return Ok(lock),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(anyhow!("Daemon socket cleanup lock: {error}")),
        }
        if attempt == LOCK_RETRIES {
            break;
        }
        tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
    }
    Err(anyhow!(
        "Timed out waiting for the daemon socket cleanup lock: {}",
        path.display()
    ))
}

/// A supervisor-lifetime proper-lockfile lease on `{socket}.lock`.
/// The opened directory pins the acquired inode across stale takeovers: a
/// displaced holder never refreshes or removes the successor's lock.
#[cfg(unix)]
#[derive(Debug)]
pub struct SocketLease {
    socket_path: std::path::PathBuf,
    lock_path: std::path::PathBuf,
    lock_dir: std::fs::File,
    identity: SocketIdentity,
    compromised: std::sync::Arc<std::sync::atomic::AtomicBool>,
    compromise_tx: tokio::sync::watch::Sender<bool>,
    refresh_stop: std::sync::mpsc::Sender<()>,
    refresh: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl SocketLease {
    /// Wait up to 15s for the exclusive lease; refresh it every second
    /// while the supervisor owns its socket, matching proper-lockfile.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or locked.
    pub async fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            crate::paths::ensure_dir(parent)?;
        }
        let lock = acquire_cleanup_lock(path).await?;
        let lock_path = lock.into_path();
        // If the open fails, ownership is unprovable; leave the artifact to
        // expire instead of risking removal of a racing successor's lock.
        let lock_dir = std::fs::File::open(&lock_path)?;
        let identity = metadata_identity(&lock_dir.metadata()?);
        let compromised = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (compromise_tx, _) = tokio::sync::watch::channel(false);
        let task_path = lock_path.clone();
        let task_dir = lock_dir.try_clone()?;
        let task_identity = identity.clone();
        let task_compromised = std::sync::Arc::clone(&compromised);
        let task_tx = compromise_tx.clone();
        let (refresh_stop, stop_rx) = std::sync::mpsc::channel();
        let refresh = std::thread::spawn(move || {
            while stop_rx.recv_timeout(Duration::from_secs(1)).is_err() {
                if !lock_identity_matches(&task_path, &task_identity)
                    || task_dir.set_modified(std::time::SystemTime::now()).is_err()
                    || !lock_identity_matches(&task_path, &task_identity)
                {
                    task_compromised.store(true, std::sync::atomic::Ordering::Release);
                    task_tx.send_replace(true);
                    break;
                }
            }
        });
        Ok(Self {
            socket_path: path.to_path_buf(),
            lock_path,
            lock_dir,
            identity,
            compromised,
            compromise_tx,
            refresh_stop,
            refresh: Some(refresh),
        })
    }

    /// Whether ownership of this exact lock inode was lost.
    #[must_use]
    pub fn compromised(&self) -> bool {
        self.compromised.load(std::sync::atomic::Ordering::Acquire)
            || !lock_identity_matches(&self.lock_path, &self.identity)
    }

    /// Resolve when this lease loses its lock directory.
    pub async fn wait_compromised(&self) {
        let mut changes = self.compromise_tx.subscribe();
        while !self.compromised() {
            if changes.changed().await.is_err() {
                break;
            }
        }
    }

    /// # Errors
    ///
    /// Returns an error if this lease was displaced or compromised.
    pub fn assert_held(&self) -> Result<()> {
        self.assert_path_held(&self.socket_path)
    }

    fn assert_path_held(&self, path: &Path) -> Result<()> {
        if path != self.socket_path {
            return Err(anyhow!(
                "Daemon socket lease does not match {}",
                path.display()
            ));
        }
        if self.compromised() {
            return Err(anyhow!(
                "Daemon socket lease for {} was compromised",
                path.display()
            ));
        }
        Ok(())
    }

    /// Best-effort unlink of only the bound socket owned by this holder.
    /// On compromise, leave the successor's socket untouched.
    pub fn cleanup_socket_path(&self, path: &Path, expected: Option<SocketIdentity>) {
        if self.assert_path_held(path).is_err() {
            return;
        }
        let Some(expected) = expected else { return };
        if socket_identity(path) == Some(expected) && self.assert_path_held(path).is_ok() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
impl Drop for SocketLease {
    fn drop(&mut self) {
        let _ = self.refresh_stop.send(());
        if let Some(refresh) = self.refresh.take() {
            let _ = refresh.join();
        }
        // The pinned fd prevents inode reuse while this lease is alive.
        // A successor that reclaimed a stale lock must never be released by us.
        if !self.compromised() {
            let _ = std::fs::remove_dir(&self.lock_path);
        }
        let _ = &self.lock_dir;
    }
}

#[cfg(unix)]
fn metadata_identity(metadata: &std::fs::Metadata) -> SocketIdentity {
    use std::os::unix::fs::MetadataExt;
    SocketIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

#[cfg(unix)]
fn lock_identity_matches(path: &Path, expected: &SocketIdentity) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_dir() && metadata_identity(&metadata) == *expected
    })
}

/// Remove a stale socket file after verifying nothing is listening.
/// Unix only: a stale socket file blocks `bind`; named pipes have no filesystem
/// residue, so preparing the path is a no-op there.
///
/// # Errors
///
/// Returns an error when the parent directory cannot be created, a live listener
/// answers, the cleanup lock cannot be acquired, or the cleanup fails.
#[cfg(unix)]
pub async fn prepare_socket_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_dir(parent)?;
    }
    // lstat, not `Path::exists()`: a dangling symlink still blocks `bind`
    // while `exists()` (which follows links) denies it.
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(anyhow!("Daemon socket path stat failed: {error}")),
        Ok(_) => {}
    }
    // Quick refusal before taking the cross-process lock: a second daemon
    // fails fast instead of queueing.
    if can_connect(path, Duration::from_millis(250)).await {
        return Err(anyhow!("Daemon socket already in use: {}", path.display()));
    }
    let _lock = acquire_cleanup_lock(path).await?;
    prepare_locked_socket_path(path, None).await
}

/// Prepare under the supervisor-lifetime lease. Unlike the short-lived
/// cleanup lock, this lease remains held through bind and the accept loop.
///
/// # Errors
///
/// Returns an error if the lease is compromised, the path is non-socket, or
/// a live listener or replaced inode prevents stale cleanup.
#[cfg(unix)]
pub async fn prepare_socket_path_with_lease(path: &Path, lease: &SocketLease) -> Result<()> {
    lease.assert_path_held(path)?;
    if let Some(parent) = path.parent() {
        crate::paths::ensure_dir(parent)?;
    }
    prepare_locked_socket_path(path, Some(lease)).await
}

/// Probe + grace wait + unlink for a probed-stale socket file; the caller owns the cleanup lock.
#[cfg(unix)]
async fn prepare_locked_socket_path(path: &Path, lease: Option<&SocketLease>) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    if let Some(lease) = lease {
        lease.assert_path_held(path)?;
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(anyhow!("Daemon socket path stat failed: {error}")),
    };
    if !metadata.file_type().is_socket() {
        return Err(anyhow!(
            "Daemon socket path exists and is not a socket: {}",
            path.display()
        ));
    }
    let stale_identity = SocketIdentity {
        dev: std::os::unix::fs::MetadataExt::dev(&metadata),
        ino: std::os::unix::fs::MetadataExt::ino(&metadata),
    };
    if can_connect(path, Duration::from_millis(250)).await {
        return Err(anyhow!("Daemon socket already in use: {}", path.display()));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
        if !path.exists() {
            return Ok(());
        }
        match socket_identity(path) {
            None => return Ok(()),
            Some(current) if current == stale_identity => {}
            Some(_) => {
                return Err(anyhow!(
                    "Daemon socket changed ownership while waiting for cleanup: {}",
                    path.display()
                ))
            }
        }
        if can_connect(path, Duration::from_millis(250)).await {
            return Err(anyhow!("Daemon socket already in use: {}", path.display()));
        }
    }
    if let Some(lease) = lease {
        lease.assert_path_held(path)?;
    }
    unlink_stale_socket_with_lease(path, stale_identity, lease).await
}

/// Final gate before unlinking a probed-stale socket file: refuse while a
/// live listener answers, and remove only the exact inode that was probed
/// stale - a file replaced between the probe and the unlink stays untouched.
/// The caller holds the cleanup lock, so competing startup workers are
/// serialized out of this check-then-act window; the identity gate covers
/// processes that do not take the lock (non-pa-daemon), like the TS gate
/// behind proper-lockfile's lease. Unix only: named-pipe endpoints leave
/// no socket file to unlink, so the whole path stays unix.
#[cfg(all(test, unix))]
async fn unlink_stale_socket(path: &Path, expected: SocketIdentity) -> Result<()> {
    unlink_stale_socket_with_lease(path, expected, None).await
}

#[cfg(unix)]
async fn unlink_stale_socket_with_lease(
    path: &Path,
    expected: SocketIdentity,
    lease: Option<&SocketLease>,
) -> Result<()> {
    if can_connect(path, Duration::from_millis(250)).await {
        return Err(anyhow!("Daemon socket already in use: {}", path.display()));
    }
    match socket_identity(path) {
        None => Ok(()),
        Some(current) if current == expected => {
            if let Some(lease) = lease {
                lease.assert_path_held(path)?;
            }
            std::fs::remove_file(path)?;
            Ok(())
        }
        Some(_) => Err(anyhow!(
            "Daemon socket changed ownership while waiting for cleanup: {}",
            path.display()
        )),
    }
}

/// Windows arm of [`prepare_socket_path`]: named-pipe endpoints have no
/// filesystem residue (the first listener creates the pipe), so preparing
/// the path is a no-op.
///
/// # Errors
///
/// Does not error: there is no path to prepare for a named pipe.
// The signature stays async for the shared unix callers (the await is
// the unix arm's own; the pipe arm has no path to prepare).
#[cfg(not(unix))]
#[cfg_attr(not(unix), allow(clippy::unused_async))]
pub async fn prepare_socket_path(_path: &Path) -> Result<()> {
    Ok(())
}

/// Remove the socket file when it still belongs to this supervisor
/// incarnation (no-op for named pipes). The remove runs best-effort under
/// the cleanup lock: contention means another daemon owns the socket path.
pub fn cleanup_socket_path(path: &Path, expected_identity: Option<SocketIdentity>) {
    if !path.exists() {
        return;
    }
    #[cfg(unix)]
    let Ok(_cleanup_lock) = pa_core::platform::LockDir::acquire(path, LOCK_STALE_AFTER) else {
        return;
    };
    if let Some(expected) = expected_identity {
        match socket_identity(path) {
            Some(current) if current == expected => {}
            _ => return,
        }
    }
    let _ = std::fs::remove_file(path);
}

/// The exit cleanup after the owner's own listener is closed (the TS
/// graceful-shutdown sequence closes before cleanup). A successor's live
/// listener must survive even when a poisoned bind-time identity capture
/// names the successor's inode. A nonblocking connect can distinguish a
/// definitely closed listener (`ECONNREFUSED`) from a saturated backlog
/// (`EAGAIN` on Linux); unknown outcomes preserve the socket path. Only
/// after definite refusal may the existing cleanup lock and identity gate
/// unlink the stale, still-ours socket. TS cleanup checks identity alone,
/// so the poisoned-capture case remains a disclosed TS difference.
#[cfg(unix)]
pub fn cleanup_socket_path_after_close(path: &Path, expected_identity: Option<SocketIdentity>) {
    if !path.exists() || !pa_types::platform::transport::unix_listener_definitely_closed(path) {
        return;
    }
    cleanup_socket_path(path, expected_identity);
}

#[cfg(not(unix))]
pub fn cleanup_socket_path_after_close(_path: &Path, _expected_identity: Option<SocketIdentity>) {}

/// Restrict the bound socket file to its owner (Unix mode 0o600; Windows
/// named pipes use ACLs on the pipe object instead).
pub fn restrict_socket_path(path: &Path) {
    let _ = pa_core::platform::perms::restrict_file(path);
}

/// The bind-capture gap seam (the `PA_DAEMON_EVENT_LOG` seam family): a
/// replacement landing between the bind and the bind-time identity capture
/// poisons the captured identity - the exact residual the
/// close-listener exit cleanup exists to survive. Production leaves the
/// gap unset, so the bind and the capture stay back-to-back; the
/// poisoned-capture oracle sets the gap so the replacement provably lands
/// in the window instead of racing microseconds.
pub const BIND_CAPTURE_GAP_ENV: &str = "PA_DAEMON_BIND_CAPTURE_GAP_MS";

/// Sleep the bounded bind-capture fault-injection gap in debug builds only.
/// Production binaries never pause startup between bind and identity capture.
pub async fn bind_capture_gap() {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var(BIND_CAPTURE_GAP_ENV) {
        if let Ok(ms @ 1..=2_000) = raw.parse::<u64>() {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use pa_types::platform::transport::bind_transport;

    /// Bind and drop the listener: the socket file outlives the fd with
    /// nobody listening - exactly a crashed worker's residue.
    async fn bind_stale_socket(path: &Path) {
        drop(bind_transport(path).await.expect("bind stale socket"));
    }

    #[tokio::test]
    async fn lifetime_lease_refuses_a_replacement_and_never_releases_successors_lock() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        let lease = SocketLease::acquire(&socket).await.unwrap();
        let lock_path = pa_core::platform::LockDir::path_for(&socket);
        assert!(lock_path.is_dir());
        let listener = bind_transport(&socket).await.unwrap();
        let own_socket = socket_identity(&socket);
        let old_dir = dir.path().join("previous.lock");
        std::fs::rename(&lock_path, &old_dir).unwrap();
        std::fs::create_dir(&lock_path).unwrap();
        assert!(lease.compromised());
        lease.cleanup_socket_path(&socket, own_socket);
        assert!(
            socket.exists(),
            "compromised holder cannot unlink its former socket"
        );
        tokio::time::timeout(Duration::from_secs(2), lease.wait_compromised())
            .await
            .unwrap();
        drop(lease);
        assert!(
            lock_path.is_dir(),
            "old lease must not remove successor lock"
        );
        drop(listener);
    }

    #[tokio::test]
    async fn lifetime_lease_guards_stale_cleanup_and_refuses_live_socket() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        bind_stale_socket(&socket).await;
        let lease = SocketLease::acquire(&socket).await.unwrap();
        prepare_socket_path_with_lease(&socket, &lease)
            .await
            .unwrap();
        let listener = bind_transport(&socket).await.unwrap();
        let error = prepare_socket_path_with_lease(&socket, &lease)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already in use"), "{error}");
        lease.cleanup_socket_path(&socket, socket_identity(&socket));
        assert!(!socket.exists());
        drop(listener);
        drop(lease);
        assert!(!pa_core::platform::LockDir::path_for(&socket).exists());
    }

    #[tokio::test]
    async fn missing_path_prepares_as_a_no_op() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        prepare_socket_path(&socket).await.unwrap();
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn a_dangling_symlink_is_rejected_as_not_a_socket() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        std::os::unix::fs::symlink(dir.path().join("missing.sock"), &socket).unwrap();
        let error = prepare_socket_path(&socket).await.unwrap_err();
        assert!(error.to_string().contains("not a socket"), "{error}");
        assert!(std::fs::symlink_metadata(&socket).is_ok());
    }

    #[tokio::test]
    async fn non_socket_file_at_the_path_is_refused_and_preserved() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        std::fs::write(&socket, b"not a socket").unwrap();
        let error = prepare_socket_path(&socket).await.unwrap_err();
        assert!(error.to_string().contains("not a socket"), "{error}");
        assert!(socket.exists());
    }

    #[tokio::test]
    async fn live_listener_is_never_unlinked() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = bind_transport(&socket).await.unwrap();
        let error = prepare_socket_path(&socket).await.unwrap_err();
        assert!(error.to_string().contains("already in use"), "{error}");
        assert!(socket.exists());
        assert!(can_connect(&socket, Duration::from_millis(250)).await);
        drop(listener);
    }

    /// Dropping the bound listener is the graceful close the exit
    /// cleanups run before their unlink (the TS `server.close` step,
    /// daemon-mode.ts:8011-8018): the bind releases (a fresh connect is
    /// refused) while the socket FILE survives (the close never
    /// unlinks), an in-flight accepted stream keeps serving across the
    /// close, and the listener's own fd is closed while the in-flight
    /// stream's fd stays open - no fd is leaked on the bound socket
    /// across the exit sequence.
    #[tokio::test]
    async fn dropping_the_listener_is_the_graceful_exit_close() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        fn fd_exists(fd: std::os::fd::RawFd) -> bool {
            fd_target(fd).is_some()
        }
        // What the fd number names (`socket:[inode]`): another test thread
        // may reuse a closed number at once, so "closed" means the number
        // no longer names this socket, not that the number is free.
        fn fd_target(fd: std::os::fd::RawFd) -> Option<std::path::PathBuf> {
            std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()
        }

        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        // One accepted connection in flight: the client connected, the
        // listener accepted; the stream must survive the listener's close.
        let mut client = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        let mut accepted = listener.accept().await.unwrap().0;
        let listener_fd = listener.as_raw_fd();
        let accepted_fd = accepted.as_raw_fd();
        let listener_target = fd_target(listener_fd);
        assert!(listener_target.is_some());
        drop(listener);
        assert!(
            fd_target(listener_fd) != listener_target,
            "the listener's fd closed at the drop: no fd leaked on the bound socket"
        );
        assert!(
            fd_exists(accepted_fd),
            "the in-flight accepted stream's fd survives the close"
        );
        assert!(socket.exists(), "the close never unlinks the file");
        assert!(
            !can_connect(&socket, Duration::from_millis(250)).await,
            "the bind released at the drop"
        );
        // The in-flight stream still moves bytes across the close.
        client.write_all(b"ping").unwrap();
        let mut buffer = [0u8; 4];
        accepted.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"ping");
        assert!(accepted.flush().await.is_ok());
    }

    /// The exit cleanup after the owner's listener closed spares a LIVE
    /// successor even when the expected identity matches that
    /// successor's file exactly - the poisoned capture a replacement
    /// landing in the bind->capture window produces - and still unlinks
    /// the dead file the matching identity describes (the still-ours
    /// direction: a respawn does not wait out the stale-socket path).
    #[tokio::test]
    async fn exit_cleanup_after_close_spares_a_live_successor_with_a_matching_identity() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        // The owner's own bind, closed exactly like the exit sequences
        // close it before their cleanup.
        let owner = bind_transport(&socket).await.unwrap();
        drop(owner);
        // The poisoned capture: the successor binds the path after the
        // owner's file is renamed aside, and the "captured" identity is
        // the successor's own file (a replacement landing in the
        // bind->capture window stores exactly this).
        let aside = dir.path().join("owner.sock");
        std::fs::rename(&socket, &aside).unwrap();
        let successor = bind_transport(&socket).await.unwrap();
        let poisoned = socket_identity(&socket).unwrap();
        cleanup_socket_path_after_close(&socket, Some(poisoned.clone()));
        assert!(
            socket.exists(),
            "a live successor is never unlinked, even with a matching identity"
        );
        assert!(can_connect(&socket, Duration::from_millis(250)).await);
        drop(successor);
        // The same matching identity now describes a dead file: the
        // probe passes it through and the gate unlinks it.
        cleanup_socket_path_after_close(&socket, Some(poisoned));
        assert!(!socket.exists(), "the dead still-ours file is unlinked");
        std::fs::remove_file(&aside).unwrap();
    }

    /// A full accept queue is not proof of a dead listener: the successor's
    /// inode can exactly match a poisoned bind-time identity capture.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn exit_cleanup_preserves_a_backlogged_successor_with_a_matching_identity() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        let owner = bind_transport(&socket).await.unwrap();
        drop(owner);
        let aside = dir.path().join("owner.sock");
        std::fs::rename(&socket, &aside).unwrap();
        let successor = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        socket2::SockRef::from(&successor).listen(1).unwrap();
        let poisoned = socket_identity(&socket).unwrap();
        let mut queued = Vec::new();
        // Never accept: hold each successful connection until the queue fills.
        for _ in 0..4 {
            match tokio::time::timeout(
                Duration::from_millis(100),
                tokio::net::UnixStream::connect(&socket),
            )
            .await
            {
                Ok(Ok(stream)) => queued.push(stream),
                _ => break,
            }
        }
        assert!(!queued.is_empty());
        assert!(
            !can_connect(&socket, Duration::from_millis(100)).await,
            "the successor's queue must be saturated for this oracle"
        );
        cleanup_socket_path_after_close(&socket, Some(poisoned.clone()));
        assert!(
            socket.exists(),
            "a live backlogged successor must not be unlinked"
        );
        drop(successor);
        cleanup_socket_path_after_close(&socket, Some(poisoned));
        assert!(
            !socket.exists(),
            "the same inode unlinks after its listener closes"
        );
        drop(queued);
        std::fs::remove_file(&aside).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn exit_cleanup_unlinks_a_closed_listener_on_a_long_socket_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let deep = dir.path().join("a".repeat(80)).join("b".repeat(80));
        std::fs::create_dir_all(&deep).unwrap();
        let socket = deep.join("daemon.sock");
        let owner = bind_transport(&socket).await.unwrap();
        let identity = socket_identity(&socket).unwrap();
        drop(owner);
        cleanup_socket_path_after_close(&socket, Some(identity));
        assert!(!socket.exists(), "deep stale path still unlinks");
    }

    #[tokio::test]
    async fn stale_socket_file_is_removed_and_the_path_rebinds() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        bind_stale_socket(&socket).await;
        assert!(socket.exists());
        prepare_socket_path(&socket).await.unwrap();
        assert!(!socket.exists(), "stale socket file must be unlinked");
        bind_transport(&socket)
            .await
            .expect("bind after stale cleanup");
    }

    #[tokio::test]
    async fn unlink_refuses_a_live_listener_even_when_marked_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = bind_transport(&socket).await.unwrap();
        let stale = socket_identity(&socket).unwrap();
        let error = unlink_stale_socket(&socket, stale).await.unwrap_err();
        assert!(error.to_string().contains("already in use"), "{error}");
        assert!(socket.exists());
        drop(listener);
    }

    #[tokio::test]
    async fn unlink_refuses_a_replaced_file_with_a_new_identity() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        bind_stale_socket(&socket).await;
        let stale = socket_identity(&socket).unwrap();
        // Move the probed file aside instead of unlinking it: its inode stays
        // allocated, so the replacement is guaranteed a different inode (a freed
        // inode could be handed straight back to the replacement).
        let aside = dir.path().join("probed.sock");
        std::fs::rename(&socket, &aside).unwrap();
        bind_stale_socket(&socket).await;
        assert_ne!(socket_identity(&socket).unwrap(), stale);
        let error = unlink_stale_socket(&socket, stale).await.unwrap_err();
        assert!(error.to_string().contains("changed ownership"), "{error}");
        assert!(socket.exists(), "the replacement socket file must survive");
        std::fs::remove_file(&aside).unwrap();
    }

    #[tokio::test]
    async fn unlink_is_a_no_op_when_the_file_is_already_gone() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        unlink_stale_socket(&socket, SocketIdentity { dev: 0, ino: 0 })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cleanup_waits_while_a_rival_startup_holds_the_lock() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        bind_stale_socket(&socket).await;
        // A rival startup worker owns the cleanup lock: a fresh empty
        // `{path}.lock` directory, exactly what LockDir::acquire sees as live.
        let rival_lock = dir.path().join("daemon.sock.lock");
        std::fs::create_dir(&rival_lock).unwrap();
        let socket_arg = socket.clone();
        let mut pending = tokio::spawn(async move { prepare_socket_path(&socket_arg).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut pending)
                .await
                .is_err()
        );
        // The rival releases: the queued cleanup proceeds and frees the lock.
        std::fs::remove_dir(&rival_lock).unwrap();
        pending.await.unwrap().unwrap();
        assert!(!socket.exists(), "stale socket file must be unlinked");
        assert!(
            !rival_lock.exists(),
            "the cleanup lock must be released after use"
        );
    }

    #[test]
    fn cleanup_is_deferred_while_a_rival_holds_the_lock() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let identity = socket_identity(&socket).unwrap();
        let rival_lock = dir.path().join("daemon.sock.lock");
        std::fs::create_dir(&rival_lock).unwrap();
        cleanup_socket_path(&socket, Some(identity.clone()));
        assert!(socket.exists());
        std::fs::remove_dir(&rival_lock).unwrap();
        cleanup_socket_path(&socket, Some(identity));
        assert!(!socket.exists());
        drop(listener);
    }

    #[tokio::test]
    async fn a_stale_cleanup_lock_of_a_crashed_holder_is_reclaimed() {
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("daemon.sock");
        bind_stale_socket(&socket).await;
        let crashed_lock = dir.path().join("daemon.sock.lock");
        std::fs::create_dir(&crashed_lock).unwrap();
        let six_seconds_ago =
            filetime::FileTime::from_system_time(std::time::SystemTime::now() - LOCK_STALE_AFTER);
        filetime::set_file_mtime(&crashed_lock, six_seconds_ago).unwrap();
        // A lock whose holder crashed (mtime past LOCK_STALE_AFTER) is reclaimed instead of
        // waiting out the full retry budget.
        tokio::time::timeout(Duration::from_secs(2), prepare_socket_path(&socket))
            .await
            .unwrap()
            .unwrap();
        assert!(!socket.exists(), "stale socket file must be unlinked");
    }
}
