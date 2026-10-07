//! POSIX containment: the command leads a fresh session (so its process group
//! is its pid and it has no controlling terminal), receives one end of a
//! socket pair as stdin (the status channel the fence script remaps), and
//! writes stdout and stderr into one pipe.

use std::io::{PipeReader, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;

use process_wrap::std::{ChildWrapper, CommandWrap, ProcessSession};
use rustix::event::{poll, PollFd, PollFlags};
use rustix::process::{kill_process_group, test_kill_process_group, Pid};

use super::Signal;

/// A spawned command: its process (owned by the watcher), the parent end of
/// its status channel, and the read end of its output pipe.
pub(crate) struct Spawned {
    pub process: Process,
    pub channel: ControlChannel,
    pub output: PipeReader,
}

impl Spawned {
    pub(crate) fn pid(&self) -> u32 {
        self.process.id()
    }

    /// The handle signals and liveness checks go through.
    pub(crate) fn control(&self) -> Control {
        Control {
            pid: self.process.id(),
        }
    }

    /// Kill a command that must not run (its gate never opened): close the
    /// channel, SIGKILL the group, and wait for the leader (bounded).
    pub(crate) fn abort(self) {
        let Spawned {
            mut process,
            channel,
            output,
        } = self;
        drop(channel);
        drop(output);
        signal_group(process.id(), Signal::KILL);
        process.wait_bounded(Duration::from_secs(5));
    }
}

/// The leader process. Waiting needs `&mut`, so the watcher owns it; signals
/// go to the group by id and need no handle.
pub(crate) struct Process {
    child: Box<dyn ChildWrapper>,
}

/// Exit of the leader: its code, or the negated signal that killed it (the
/// kernel's `Popen.returncode` convention).
pub(crate) type ExitCode = i32;

impl Process {
    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }

    /// Block until the leader exits.
    pub(crate) fn wait(&mut self) -> ExitCode {
        use std::os::unix::process::ExitStatusExt;
        match self.child.inner_mut().wait() {
            Ok(status) => status
                .code()
                .or_else(|| status.signal().map(|signal| -signal))
                .unwrap_or(-1),
            Err(_) => -1,
        }
    }

    /// Bounded wait used when a spawn is aborted.
    pub(crate) fn wait_bounded(&mut self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if matches!(self.child.inner_mut().try_wait(), Ok(Some(_)) | Err(_)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// The parent end of the status channel plus the wake pipe that unblocks the
/// status read when the shell dies without writing one (background children
/// can hold the socket open past the shell's lifetime).
pub(crate) struct ControlChannel {
    status: UnixStream,
    wake_read: PipeReader,
    wake_write: Option<std::io::PipeWriter>,
}

impl ControlChannel {
    /// Open the gate: the child does not run the command until this byte
    /// arrives (it is sent once the pid is journaled). A failed write means
    /// the child already died; the status and exit paths report that.
    pub(crate) fn open_gate(&self) {
        let _ = (&self.status).write_all(b"\n");
    }

    /// The writer the watcher pokes once the shell has exited.
    pub(crate) fn take_waker(&mut self) -> Option<std::io::PipeWriter> {
        self.wake_write.take()
    }

    /// Read the foreground status line. Status bytes win over the wake: any
    /// status write happens before the shell exits, so it is already readable
    /// whenever the wake fires. `None` when the shell died without one.
    pub(crate) fn read_status(mut self) -> Option<i32> {
        let mut line = Vec::new();
        while !line.contains(&b'\n') {
            let (status_ready, wake_ready) = {
                let mut fds = [
                    PollFd::new(&self.status, PollFlags::IN),
                    PollFd::new(&self.wake_read, PollFlags::IN),
                ];
                if poll(&mut fds, None).is_err() {
                    return None;
                }
                (!fds[0].revents().is_empty(), !fds[1].revents().is_empty())
            };
            if !status_ready {
                let _ = wake_ready;
                return None;
            }
            let mut chunk = [0u8; 64];
            match self.status.read(&mut chunk) {
                Ok(0) | Err(_) => return None,
                Ok(read) => line.extend_from_slice(&chunk[..read]),
            }
        }
        parse_status(&line)
    }
}

/// Python `int(line)`: surrounding whitespace is allowed.
fn parse_status(line: &[u8]) -> Option<i32> {
    std::str::from_utf8(line).ok()?.trim().parse().ok()
}

/// Spawn `command` (program, arguments, cwd and environment set) contained
/// in its own session, gated on the status channel.
///
/// # Errors
///
/// The OS error of the pipe, socket or spawn.
pub(crate) fn spawn(mut command: Command) -> std::io::Result<Spawned> {
    let (parent, child_end) = UnixStream::pair()?;
    let (wake_read, wake_write) = std::io::pipe()?;
    let (output, output_write) = std::io::pipe()?;
    command
        .stdin(Stdio::from(OwnedFd::from(child_end)))
        .stdout(Stdio::from(output_write.try_clone()?))
        .stderr(Stdio::from(output_write));
    let mut wrapped = CommandWrap::from(command);
    wrapped.wrap(ProcessSession);
    let child = wrapped.spawn()?;
    // The command (and with it the parent's copies of the child-side fds)
    // closes here, so the output pipe reports EOF once the tree closes it.
    drop(wrapped);
    Ok(Spawned {
        process: Process { child },
        channel: ControlChannel {
            status: parent,
            wake_read,
            wake_write: Some(wake_write),
        },
        output,
    })
}

/// Signals and liveness for a command's process group, by its leader's pid
/// (the group id): usable while the watcher owns the leader.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Control {
    pid: u32,
}

impl Control {
    pub(crate) fn signal(self, signal: Signal) -> bool {
        signal_group(self.pid, signal)
    }

    pub(crate) fn group_alive(self) -> bool {
        group_alive(self.pid)
    }

    /// After the leader exited: kill members it left behind. True when the
    /// group is gone or the kill was delivered (the record may go inactive).
    pub(crate) fn reap(self) -> bool {
        if !group_alive(self.pid) {
            return true;
        }
        signal_group(self.pid, Signal::KILL)
    }
}

/// Whether the platform delivers the foreground status on its own channel
/// (the fence script); without one the result is final at shell exit.
pub(crate) const STATUS_CHANNEL: bool = true;

fn group(pid: u32) -> Option<Pid> {
    i32::try_from(pid).ok().and_then(Pid::from_raw)
}

/// Signal the process group led by `pid`. True when the signal was
/// delivered or the group is already gone (safe to record inactive); false
/// when it was not delivered (the record must stay active for the reaper).
fn signal_group(pid: u32, signal: Signal) -> bool {
    let (Some(group), Some(signal)) = (
        group(pid),
        rustix::process::Signal::from_named_raw(signal.0),
    ) else {
        return false;
    };
    match kill_process_group(group, signal) {
        Ok(()) => true,
        Err(error) => error == rustix::io::Errno::SRCH,
    }
}

/// Whether any member of the group led by `pid` is alive (a member this
/// process may not signal still counts).
fn group_alive(pid: u32) -> bool {
    let Some(group) = group(pid) else {
        return false;
    };
    match test_kill_process_group(group) {
        Ok(()) => true,
        Err(error) => error != rustix::io::Errno::SRCH,
    }
}

/// Bytes waiting in the output pipe that the pump has not read yet.
pub(crate) fn pending_bytes(output: &impl AsFd) -> bool {
    rustix::io::ioctl_fionread(output).is_ok_and(|pending| pending > 0)
}

/// Wait until the pipe is readable (data or EOF).
pub(crate) fn wait_readable(output: &impl AsFd) -> bool {
    let mut fds = [PollFd::new(output, PollFlags::IN)];
    poll(&mut fds, None).is_ok()
}
