//! The one lock of a `harness_state.json` file (`{file}.lock`). Every
//! writer of the file takes it through [`lock_harness_state_file`]: the
//! kernel's harness store, refine, the RAVO commit and the failure-ledger
//! flush, so they share one owner protocol and one wait policy.
//!
//! The lock is owned (a live holder is never reclaimed, only a provably
//! dead one past the stale window) and heartbeated (a slow live holder
//! keeps it fresh). A writer waits per holder, not per queue: each time
//! the lock changes hands the wait restarts, so a convoy of writers that
//! each hold briefly never times a write out; only one holder keeping it
//! past the wait does.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::platform::{HeartbeatLock, LockDir};

/// How a writer takes a harness state lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessLockPolicy {
    /// How long a writer waits for one holder: past the stale window, so a
    /// crashed holder's leftover is always reclaimed first.
    pub wait: Duration,
    /// The pause between acquisition attempts.
    pub retry: Duration,
    /// A lock this old whose owner is gone is a crashed holder's leftover
    /// (the TS host's `HARNESS_STATE_LOCK_STALE_MS`).
    pub stale: Duration,
}

/// The policy every `harness_state.json` writer uses.
pub const HARNESS_STATE_LOCK: HarnessLockPolicy = HarnessLockPolicy {
    wait: Duration::from_secs(15),
    retry: Duration::from_millis(5),
    stale: Duration::from_secs(10),
};

/// Take the lock of the harness state file at `state_path`, creating its
/// directory. Blocking; call it off the async runtime. Dropping the guard
/// stops the heartbeat and releases the lock.
///
/// # Errors
///
/// [`io::ErrorKind::TimedOut`] when one holder kept the lock longer than
/// `policy.wait`, or the I/O error that kept the lock from being made.
pub fn lock_harness_state_file(
    state_path: &Path,
    policy: HarnessLockPolicy,
) -> io::Result<HeartbeatLock> {
    if let Some(parent) = state_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = LockDir::path_for(state_path);
    let mut holder = None;
    let mut deadline = Instant::now() + policy.wait;
    loop {
        match LockDir::acquire_owned(state_path, policy.stale) {
            Ok(held) => return Ok(held.with_heartbeat(policy.stale / 2)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let current = LockDir::holder_at(&lock_path);
                if current.is_some() && current != holder {
                    holder = current;
                    deadline = Instant::now() + policy.wait;
                } else if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "harness state is locked by another process: {} (held longer than {}s)",
                            lock_path.display(),
                            policy.wait.as_secs()
                        ),
                    ));
                }
                std::thread::sleep(policy.retry);
            }
            Err(error) => return Err(error),
        }
    }
}
