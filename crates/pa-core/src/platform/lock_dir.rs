//! Cross-process directory locks, byte-compatible with the TS product's
//! `proper-lockfile` 4.1.2 convention: a lock is an EMPTY DIRECTORY at
//! `{file}.lock`, staleness is judged from its bumped mtime alone (no pid
//! or owner file), and a regular file at the lock path is removed on acquisition.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Minimum staleness threshold, like proper-lockfile's floor.
const MIN_STALE: Duration = Duration::from_secs(2);

/// The mtime bump proper-lockfile's precision probe writes: the next whole
/// second plus 5ms, so a millisecond-precision filesystem records a time
/// that is "not on the second".
fn probe_mtime() -> (i64, i64) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    let seconds = (now_ms + 999).div_euclid(1000);
    (seconds, 5_000_000)
}

// The `libc::timespec` field names are the syscall's own vocabulary; the struct-literal shorthand
// below is the point.
#[allow(clippy::similar_names)]
#[cfg(unix)]
fn set_mtime(path: &Path, tv_sec: i64, tv_nsec: i64) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    // Lock paths come from agent-dir joins, but keep the NUL case an error
    // instead of truncating the path inside libc.
    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let times = [
        libc::timespec { tv_sec, tv_nsec },
        libc::timespec { tv_sec, tv_nsec },
    ];
    // Relative lock paths (a relative agent dir) resolve against the
    // process cwd through AT_FDCWD.
    let result = unsafe { libc::utimensat(libc::AT_FDCWD, path_c.as_ptr(), times.as_ptr(), 0) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Windows: set the directory's last-write time via `CreateFileW` (with
/// `FILE_FLAG_BACKUP_SEMANTICS`, the only way to open a directory) +
/// `SetFileTime` - the mtime probe `utimensat` performs on Unix.
// The `libc::timespec` field names are the syscall's own vocabulary; the struct-literal shorthand
// below is the point.
#[allow(clippy::similar_names)]
#[cfg(windows)]
fn set_mtime(path: &Path, tv_sec: i64, tv_nsec: i64) -> io::Result<()> {
    win32::set_last_write_time(path, tv_sec, tv_nsec)
}

#[cfg(not(any(unix, windows)))]
fn set_mtime(_path: &Path, _tv_sec: i64, _tv_nsec: i64) -> io::Result<()> {
    Err(io::Error::other(
        "directory lock mtime probe is not implemented on this platform",
    ))
}

/// The kernel32 file-time surface for the lock probe, hand-declared (repo
/// policy: pinned constants/externs, no windows-sys dependency).
#[cfg(windows)]
mod win32 {
    #![allow(non_snake_case)]

    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    /// `winbase.h`: required to open a directory handle.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    /// `winbase.h`: write access to the file's times.
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
    /// `winnt.h` `FILE_SHARE_READ | WRITE | DELETE`: a concurrent stat of the lock dir must not be
    /// blocked.
    const FILE_SHARE_ALL: u32 = 0x0000_0007;
    /// `winbase.h` `OPEN_EXISTING`.
    const OPEN_EXISTING: u32 = 3;
    /// `winbase.h`: `CreateFileW` returns this (not null) on failure.
    const INVALID_HANDLE_VALUE: isize = -1;

    /// A Win32 `FILETIME`: 100ns ticks since 1601-01-01 UTC, split 32/32.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FileTime {
        dwLowDateTime: u32,
        dwHighDateTime: u32,
    }

    type Handle = *mut c_void;

    extern "system" {
        fn CreateFileW(
            filename: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: Handle,
        ) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn SetFileTime(
            handle: Handle,
            creation_time: *const FileTime,
            last_access_time: *const FileTime,
            last_write_time: *const FileTime,
        ) -> i32;
    }

    /// `(tv_sec, tv_nsec)` -> FILETIME. The Windows epoch trails the Unix
    /// epoch by 11644473600 seconds; the sub-second part is nanoseconds
    /// against FILETIME's 100ns ticks.
    // `tv_sec`/`tv_nsec` are the POSIX timespec spellings the callers
    // pass through; the pair is the domain's own vocabulary.
    #[allow(clippy::similar_names)]
    fn unix_to_filetime(tv_sec: i64, tv_nsec: i64) -> FileTime {
        const EPOCH_DELTA_TICKS: i64 = 11_644_473_600 * 10_000_000;
        let ticks = tv_sec * 10_000_000 + EPOCH_DELTA_TICKS + tv_nsec / 100;
        FileTime {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        }
    }

    /// Set the directory's last-write time. The `tv_sec`/`tv_nsec` pair
    /// is the POSIX timespec vocabulary, same as `unix_to_filetime`.
    #[allow(clippy::similar_names)]
    pub(crate) fn set_last_write_time(path: &Path, tv_sec: i64, tv_nsec: i64) -> io::Result<()> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_WRITE_ATTRIBUTES,
                FILE_SHARE_ALL,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle as isize == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let last_write = unix_to_filetime(tv_sec, tv_nsec);
        let ok = unsafe {
            SetFileTime(
                handle,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::from_ref(&last_write),
            )
        };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// An exclusive cross-process lock on `{path}.lock`, released on drop by
/// removing the directory.
#[derive(Debug)]
pub struct LockDir {
    path: PathBuf,
}

impl LockDir {
    /// Lock path for the guarded file.
    #[must_use]
    pub fn path_for(file: &Path) -> PathBuf {
        let mut path = file.as_os_str().to_os_string();
        path.push(".lock");
        PathBuf::from(path)
    }

    /// Acquire exclusively: create `{file}.lock` as an empty directory and
    /// bump its mtime. A fresh foreign lock surfaces as
    /// [`io::ErrorKind::WouldBlock`] (the TS protocol's ELOCKED); a lock
    /// older than `stale_after` is removed and retried once.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::WouldBlock`] for a fresh foreign lock; other I/O errors as-is.
    pub fn acquire(file: &Path, stale_after: Duration) -> io::Result<Self> {
        Self::acquire_at(&Self::path_for(file), stale_after)
    }

    /// [`LockDir::acquire`] at an explicit lock-directory path - for
    /// protocols that name the lock directory itself (the TS supervisor
    /// registry guard locks its directory at `<registryDir>/.guard`, not
    /// at `<file>.lock`), so a rust process and a TS process serialize on
    /// the SAME on-disk lock.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::WouldBlock`] when a fresh lock is held by
    /// another process, and any underlying I/O error (missing parent,
    /// permissions, stale-reclaim failures) as-is.
    pub fn acquire_at(path: &Path, stale_after: Duration) -> io::Result<Self> {
        let path = path.to_path_buf();
        let stale_after = stale_after.max(MIN_STALE);
        match Self::create(&path) {
            Ok(()) => Ok(LockDir { path }),
            // Only an existing path is a lock collision; other failures
            // (missing parent, permissions) are real errors, never contention.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Self::judge_and_reclaim(&path, stale_after)?;
                // The judge path removed (or raced away) the incumbent: one
                // fresh attempt; a reappearing rival is contention.
                match Self::create(&path) {
                    Ok(()) => Ok(LockDir { path }),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            format!("Lock file is already being held: {}", path.display()),
                        ))
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// The mkdir is the acquisition signal: EEXIST is the only collision.
    #[cfg(unix)]
    fn create(path: &Path) -> io::Result<()> {
        fs::create_dir(path)?;
        let (sec, nanos) = probe_mtime();
        if let Err(error) = set_mtime(path, sec, nanos) {
            // Never leave a lock artifact behind a failed probe.
            let _ = fs::remove_dir(path);
            return Err(error);
        }
        Ok(())
    }

    /// The mkdir is the acquisition signal; the mtime probe keeps staleness
    /// meaningful on NTFS (directory mtimes otherwise sit on the second).
    #[cfg(windows)]
    fn create(path: &Path) -> io::Result<()> {
        fs::create_dir(path)?;
        let (sec, nanos) = probe_mtime();
        if let Err(error) = set_mtime(path, sec, nanos) {
            // Never leave a lock artifact behind a failed probe.
            let _ = fs::remove_dir(path);
            return Err(error);
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn create(path: &Path) -> io::Result<()> {
        // No mtime probe on this platform: staleness is judged from the
        // filesystem's own directory mtime.
        fs::create_dir(path)
    }

    /// Decide the fate of an incumbent at `path`. Returns only when the
    /// incumbent was removed (or vanished) and acquisition may be retried;
    /// surfaces `WouldBlock` while a live or not-yet-stale lock holds it.
    fn judge_and_reclaim(path: &Path, stale_after: Duration) -> io::Result<()> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            // Removed meanwhile: retry the create.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata.is_file() {
            // A regular file is not a lock in this protocol (a pre-compat
            // Rust build or foreign artifact): remove it and retry - but
            // only when no live flock holder guards it.
            #[cfg(unix)]
            {
                if Self::legacy_flock_held(path) {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!("Lock file is already being held: {}", path.display()),
                    ));
                }
            }
            match fs::remove_file(path) {
                Ok(()) => return Ok(()),
                // A racing reclaim removed it first: retry the create.
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        if metadata.is_dir() {
            let modified = metadata.modified()?;
            let age = std::time::SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default();
            if age > stale_after {
                // Stale: remove and let the caller retry.
                match fs::remove_dir(path) {
                    Ok(()) => return Ok(()),
                    // A racing holder released it first.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                    Err(error) => return Err(error),
                }
            }
        }
        // Live lock: contention.
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("Lock file is already being held: {}", path.display()),
        ))
    }

    /// True while another process holds the pre-compat flock on a legacy
    /// lock FILE. Its absence (or an unopenable path) means nobody guards
    /// it, so the artifact can be reclaimed safely.
    #[cfg(unix)]
    fn legacy_flock_held(path: &Path) -> bool {
        use std::os::unix::io::AsRawFd;
        let Ok(file) = fs::OpenOptions::new().write(true).open(path) else {
            return false;
        };
        (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
            && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock)
    }

    /// Transfer the acquired lock path to a caller that manages ownership
    /// and release itself (for long-lived leases with inode-guarded cleanup).
    #[must_use]
    pub fn into_path(self) -> PathBuf {
        let mut this = std::mem::ManuallyDrop::new(self);
        std::mem::take(&mut this.path)
    }

    /// Release: remove the lock directory. A missing directory means someone
    /// else already reclaimed it (the TS release tolerates ENOENT); other
    /// failures go to the trace log (`Drop` cannot propagate).
    pub fn release(&self) {
        if let Err(error) = fs::remove_dir(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!("failed to release lock {}: {error}", self.path.display());
            }
        }
    }
}

impl Drop for LockDir {
    fn drop(&mut self) {
        self.release();
    }
}

/// Which acquisition holds the lock at a path: a re-created lock directory
/// is a new holder even where the filesystem reuses the inode, and a
/// heartbeat (an mtime bump) is the same holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockHolder {
    created: std::time::SystemTime,
    #[cfg(unix)]
    inode: (u64, u64),
}

impl LockDir {
    /// The holder of the lock directory at `path` (see
    /// [`LockDir::path_for`]), or `None` when there is none or the
    /// filesystem does not record creation times.
    #[must_use]
    pub fn holder_at(path: &Path) -> Option<LockHolder> {
        let metadata = fs::symlink_metadata(path).ok()?;
        Some(LockHolder {
            created: metadata.created().ok()?,
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt as _;
                (metadata.dev(), metadata.ino())
            },
        })
    }

    /// Bump the lock's mtime (proper-lockfile's `update`), so a holder that
    /// is alive but slow is never judged stale.
    ///
    /// # Errors
    ///
    /// The lock directory is gone (reclaimed by another process) or its
    /// times cannot be set.
    pub fn refresh(&self) -> io::Result<()> {
        let (sec, nanos) = probe_mtime();
        set_mtime(&self.path, sec, nanos)
    }

    /// Hold the lock with a heartbeat: a thread refreshes it every
    /// `interval` until the returned guard drops, which stops the thread
    /// and then releases the lock. For a holder whose critical section can
    /// outlast the stale window (a synced write under I/O pressure).
    #[must_use]
    pub fn with_heartbeat(self, interval: Duration) -> HeartbeatLock {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let lock = std::sync::Arc::new(self);
        let beating = std::sync::Arc::clone(&lock);
        let thread = std::thread::Builder::new()
            .name("lock-heartbeat".to_string())
            .spawn(move || {
                while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                    stopped.recv_timeout(interval)
                {
                    if let Err(error) = beating.refresh() {
                        tracing::warn!(
                            "failed to refresh lock {}: {error}",
                            beating.path.display()
                        );
                    }
                }
            });
        match thread {
            Ok(thread) => HeartbeatLock {
                stop: Some(stop),
                thread: Some(thread),
                lock: Some(lock),
            },
            Err(error) => {
                // No heartbeat thread: the lock is still held, only
                // unrefreshed (the behaviour before heartbeats).
                tracing::warn!("failed to start the lock heartbeat: {error}");
                HeartbeatLock {
                    stop: None,
                    thread: None,
                    lock: Some(lock),
                }
            }
        }
    }
}

/// A [`LockDir`] kept fresh by a heartbeat thread; see
/// [`LockDir::with_heartbeat`].
#[derive(Debug)]
pub struct HeartbeatLock {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    lock: Option<std::sync::Arc<LockDir>>,
}

impl Drop for HeartbeatLock {
    fn drop(&mut self) {
        // Stop the heartbeat before the release, so it never bumps a lock
        // directory another process has created since.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        drop(self.lock.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_of(file: &Path) -> PathBuf {
        LockDir::path_for(file)
    }

    #[test]
    fn lock_is_an_empty_directory_and_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        std::fs::write(&file, "{}").unwrap();
        {
            let _guard = LockDir::acquire(&file, MIN_STALE).unwrap();
            let path = lock_of(&file);
            let metadata = std::fs::metadata(&path).unwrap();
            assert!(metadata.is_dir(), "lock must be a directory");
            assert!(std::fs::read_dir(&path).unwrap().next().is_none());
        }
        assert!(!lock_of(&file).exists(), "release removes the directory");
    }

    #[test]
    fn acquire_at_locks_the_exact_named_path() {
        // The TS supervisor registry guards its directory with a lock
        // directory named exactly `<registryDir>/.guard` (proper-lockfile's
        // lockfilePath), so a rust visitor must be able to take the same
        // on-disk lock - not the `{file}.lock` convention - with the same
        // empty-directory body and the same off-second mtime probe a TS
        // holder writes (byte-compatibility both directions).
        let dir = tempfile::tempdir().unwrap();
        let guard = dir.path().join(".guard");
        {
            let _held = LockDir::acquire_at(&guard, MIN_STALE).unwrap();
            assert!(guard.is_dir(), "the named path itself is the lock");
            assert!(
                std::fs::read_dir(&guard).unwrap().next().is_none(),
                "the lock body is the empty directory proper-lockfile writes"
            );
            assert!(!lock_of(&guard).exists(), "no .lock twin is created");
            let modified = std::fs::metadata(&guard)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            assert_eq!(
                modified.as_millis() % 1000,
                5,
                "the same ceil-plus-5ms probe a TS holder's lock carries"
            );
            assert!(
                LockDir::acquire_at(&guard, MIN_STALE).is_err(),
                "a fresh named lock is contention"
            );
        }
        assert!(!guard.exists(), "release removes the named lock");
    }

    #[test]
    fn mtime_matches_the_proper_lockfile_probe_shape() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        std::fs::write(&file, "{}").unwrap();
        let _guard = LockDir::acquire(&file, MIN_STALE).unwrap();
        let modified = std::fs::metadata(lock_of(&file))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();

        assert_eq!(modified.as_millis() % 1000, 5);
        assert!(
            modified.as_millis()
                >= std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis()
        );
    }

    #[test]
    fn second_acquire_reports_contention() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, "{}").unwrap();
        let _guard = LockDir::acquire(&file, MIN_STALE).unwrap();
        let error = LockDir::acquire(&file, MIN_STALE).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn stale_lock_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        std::fs::write(&file, "{}").unwrap();
        let stale = lock_of(&file);
        std::fs::create_dir(&stale).unwrap();
        // Age it past the staleness threshold.
        set_mtime(&stale, 1, 0).unwrap();
        let guard = LockDir::acquire(&file, MIN_STALE).unwrap();
        let metadata = std::fs::metadata(&stale).unwrap();
        assert!(metadata.is_dir());
        drop(guard);
        assert!(!stale.exists());
    }

    /// A heartbeat keeps a held lock fresh past the stale window, and its
    /// guard stops the heartbeat and releases the lock.
    #[test]
    fn a_heartbeat_keeps_the_lock_fresh_until_its_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        let lock = lock_of(&file);
        let held = LockDir::acquire(&file, MIN_STALE).unwrap();
        // Age it as a holder past the window would be without a heartbeat.
        set_mtime(&lock, 1, 0).unwrap();
        let held = held.with_heartbeat(Duration::from_millis(10));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::fs::metadata(&lock)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .is_ok_and(|age| age > MIN_STALE)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the heartbeat never refreshed the lock"
            );
            std::thread::yield_now();
        }
        assert!(
            LockDir::acquire(&file, MIN_STALE).is_err(),
            "a refreshed lock is live, not reclaimable"
        );
        drop(held);
        assert!(!lock.exists(), "the guard releases the lock");
    }

    /// A refresh keeps the holder; a released and re-created lock is a new one.
    #[test]
    fn the_holder_survives_a_refresh_and_changes_on_reacquisition() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state.json");
        let lock = lock_of(&file);
        assert_eq!(LockDir::holder_at(&lock), None, "no lock, no holder");
        let first = LockDir::acquire(&file, MIN_STALE).unwrap();
        let Some(holder) = LockDir::holder_at(&lock) else {
            // The filesystem records no creation times: nothing to compare.
            return;
        };
        first.refresh().unwrap();
        assert_eq!(LockDir::holder_at(&lock), Some(holder));
        drop(first);
        let _second = LockDir::acquire(&file, MIN_STALE).unwrap();
        let next = LockDir::holder_at(&lock);
        assert!(
            next.is_some() && next != Some(holder),
            "{next:?} vs {holder:?}"
        );
    }

    #[test]
    fn legacy_lock_file_is_removed_not_choked_on() {
        // A pre-compat Rust build left flock FILES at the lock path (the TS
        // product rmdir()s them and dies with ENOTDIR): acquisition must heal.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth.json");
        std::fs::write(&file, "{}").unwrap();
        std::fs::write(lock_of(&file), "legacy flock artifact").unwrap();
        let guard = LockDir::acquire(&file, MIN_STALE).unwrap();
        assert!(std::fs::metadata(lock_of(&file)).unwrap().is_dir());
        drop(guard);
        assert!(!lock_of(&file).exists());
    }
}
