//! Process containment per platform: a command's whole process tree must be
//! signalable and reapable as one unit. POSIX puts the command in a session
//! (and process group) of its own; Windows puts it in a kill-on-close job
//! object while it is still suspended, so no descendant can escape it.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::{
    has_controlling_terminal, pending_bytes, spawn, wait_readable, Control, ControlChannel,
    Process, Spawned, STATUS_CHANNEL,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    has_controlling_terminal, pending_bytes, spawn, wait_readable, Control, ControlChannel,
    Process, Spawned, STATUS_CHANNEL,
};

/// How a command's process tree is kept together as one signalable unit
/// (POSIX; Windows always uses a job object).
///
/// A command must not keep the host's controlling terminal: a background
/// group that reads it stops on SIGTTIN, and one that opens `/dev/tty`
/// prompts the user's terminal. Without a terminal to escape, its own
/// process group is the whole contract (the group id is its pid), and std
/// spawns that with `posix_spawn`, whose cost does not grow with the host's
/// memory. With one, the command leads a new session: through the exec'd
/// launcher when the host registered one, else set up between fork and exec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Containment {
    /// The host has no controlling terminal: a new process group.
    ProcessGroup,
    /// The launcher calls `setsid` and acknowledges on stdin before exec.
    LauncherSession,
    /// `setsid` in the forked child (no launcher registered).
    ForkSession,
}

/// A signal a caller asks a command's group to receive. Windows has no
/// signals: every one terminates the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Signal(pub i32);

impl Signal {
    pub(crate) const TERM: Signal = Signal(15);
    pub(crate) const KILL: Signal = Signal(9);
}
