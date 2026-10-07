//! Process containment per platform: a command's whole process tree must be
//! signalable and reapable as one unit. POSIX puts the command in a session
//! (and process group) of its own; Windows puts it in a kill-on-close job
//! object while it is still suspended, so no descendant can escape it.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::{
    pending_bytes, spawn, wait_readable, Control, ControlChannel, Process, Spawned, STATUS_CHANNEL,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    pending_bytes, spawn, wait_readable, Control, ControlChannel, Process, Spawned, STATUS_CHANNEL,
};

/// A signal a caller asks a command's group to receive. Windows has no
/// signals: every one terminates the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Signal(pub i32);

impl Signal {
    pub(crate) const TERM: Signal = Signal(15);
    pub(crate) const KILL: Signal = Signal(9);
}
