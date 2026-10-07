//! The only `unsafe` in this crate: the Landlock ABI probe and the
//! fork/exec hook that confines a child.
//!
//! Why not a safe API: `landlock::RulesetCreated::restrict_self` confines
//! the *calling* process, and it consumes the ruleset, so it can neither
//! run in the parent (the worker would confine itself) nor be retried by a
//! second spawn of the same command; `std::os::unix::process::CommandExt`
//! offers no confinement hook other than `pre_exec`, which is `unsafe`
//! because the closure runs in a forked copy of a multithreaded process.
//! The alternative, re-executing our own binary as a confining launcher,
//! needs the `prime-agent` binary on hand in every caller (the library
//! tests have none).
//!
//! So the hook stays minimal: everything that allocates is built before the
//! fork (`Restriction::new` holds the ruleset descriptor and the BPF
//! instructions), and the child runs exactly three syscalls (`prctl`,
//! `landlock_restrict_self`, `seccomp`) on that prepared data. Errors are
//! raw `errno` values (`io::Error::last_os_error` does not allocate).

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Arc;

/// `landlock_create_ruleset(2)` flag: return the highest supported ABI.
const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;

/// The running kernel's Landlock ABI (`<= 0`: not built in or disabled).
pub(super) fn landlock_abi() -> i32 {
    // SAFETY: with a null attribute pointer, a zero size and the VERSION
    // flag, the syscall reads no memory and only returns the ABI number
    // (or -1 with errno ENOSYS/EOPNOTSUPP).
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    i32::try_from(version).unwrap_or(-1)
}

/// What a child applies to itself before exec: a Landlock ruleset (the
/// descriptor stays open in the parent for later spawns; it is
/// close-on-exec) and an optional seccomp program.
pub(super) struct Restriction {
    ruleset: OwnedFd,
    socket_filter: Option<Vec<libc::sock_filter>>,
}

impl Restriction {
    pub(super) fn new(ruleset: OwnedFd, socket_filter: Option<Vec<libc::sock_filter>>) -> Self {
        Self {
            ruleset,
            socket_filter,
        }
    }

    /// Runs in the forked child: no allocation, no locks, syscalls only.
    fn apply_in_child(&self) -> io::Result<()> {
        // SAFETY: prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) takes no pointers;
        // it is async-signal-safe and required before an unprivileged
        // process may restrict itself.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor is a valid Landlock ruleset owned by
        // `self`, which the closure keeps alive; flags 0 is the only
        // defined value. The syscall reads no user memory.
        let restricted = unsafe {
            libc::syscall(
                libc::SYS_landlock_restrict_self,
                self.ruleset.as_raw_fd(),
                0u32,
            )
        };
        if restricted != 0 {
            return Err(io::Error::last_os_error());
        }
        if let Some(filter) = &self.socket_filter {
            let program = libc::sock_fprog {
                len: u16::try_from(filter.len())
                    .map_err(|_| io::Error::from_raw_os_error(libc::E2BIG))?,
                filter: filter.as_ptr().cast_mut(),
            };
            // SAFETY: `program` points at `filter`'s instructions, alive
            // for the call; the kernel copies them (`copy_from_user`) and
            // never writes through the pointer. Building the descriptor on
            // the stack allocates nothing.
            let installed = unsafe {
                libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER,
                    0u32,
                    std::ptr::from_ref(&program),
                )
            };
            if installed != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

/// Install the hook: every spawn of `command` confines its child between
/// fork and exec; a failure aborts that spawn with the syscall's errno.
pub(super) fn confine_on_exec(command: &mut Command, restriction: Arc<Restriction>) {
    // SAFETY: the closure runs in the child after fork and before exec. It
    // only reads `restriction` (built before the fork, immutable, kept
    // alive by the closure's Arc) and makes the async-signal-safe syscalls
    // in `apply_in_child`: no allocation, no lock, no other thread's state.
    unsafe {
        command.pre_exec(move || restriction.apply_in_child());
    }
}
