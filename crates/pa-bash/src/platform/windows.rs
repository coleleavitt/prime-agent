//! Windows containment: one kill-on-close job object per command, entered
//! while the child is still suspended (process-wrap's `JobObject`), so the
//! child and every descendant die when the job is terminated or its last
//! handle closes, including when this process dies. Windows has no
//! foreground-status channel: the command runs as written and its result is
//! final at shell exit (the drain after it is best-effort).

use std::collections::BTreeMap;
use std::io::PipeReader;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use process_wrap::std::{ChildWrapper, CommandWrap, JobObject};

use super::Signal;

/// Whether the platform delivers the foreground status on its own channel.
pub(crate) const STATUS_CHANNEL: bool = false;

/// How often the watcher looks for the leader's exit: waiting needs the job
/// handle's lock, which a kill must be able to take meanwhile.
const EXIT_POLL: Duration = Duration::from_millis(20);

type Shared = Arc<Mutex<Box<dyn ChildWrapper>>>;

fn lock(child: &Shared) -> MutexGuard<'_, Box<dyn ChildWrapper>> {
    child
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) struct Spawned {
    pub process: Process,
    pub channel: ControlChannel,
    pub output: PipeReader,
}

impl Spawned {
    pub(crate) fn pid(&self) -> u32 {
        self.process.pid
    }

    pub(crate) fn control(&self) -> Control {
        Control {
            child: Arc::clone(&self.process.child),
        }
    }

    /// Terminate a command that must not run: its job dies with every
    /// process in it.
    pub(crate) fn abort(self) {
        let mut child = lock(&self.process.child);
        let _ = child.start_kill();
        let _ = child.wait();
    }
}

/// The leader, waited for by the watcher.
pub(crate) struct Process {
    pid: u32,
    child: Shared,
}

impl Process {
    pub(crate) fn id(&self) -> u32 {
        self.pid
    }

    /// Block until the leader exits; its exit code (a terminated job reports
    /// the code it was terminated with).
    pub(crate) fn wait(&mut self) -> i32 {
        loop {
            match lock(&self.child).inner_mut().try_wait() {
                Ok(Some(status)) => return status.code().unwrap_or(-1),
                Ok(None) => std::thread::sleep(EXIT_POLL),
                Err(_) => return -1,
            }
        }
    }
}

/// No status channel on Windows: the gate is the job assignment itself (the
/// child stays suspended until it is inside its job).
pub(crate) struct ControlChannel;

#[expect(
    clippy::unused_self,
    reason = "the POSIX status channel's interface, with nothing to do"
)]
impl ControlChannel {
    pub(crate) fn open_gate(&self) {}

    pub(crate) fn take_waker(&mut self) -> Option<std::io::PipeWriter> {
        None
    }

    pub(crate) fn read_status(self) -> Option<i32> {
        None
    }
}

/// Termination and liveness through the job.
#[derive(Clone)]
pub(crate) struct Control {
    child: Shared,
}

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control").finish_non_exhaustive()
    }
}

impl Control {
    /// Every signal terminates the job (Windows has no signals).
    pub(crate) fn signal(&self, _signal: Signal) -> bool {
        lock(&self.child).start_kill().is_ok()
    }

    /// Whether the leader still runs. Descendants that outlive it are
    /// terminated with the job at the reap.
    pub(crate) fn group_alive(&self) -> bool {
        matches!(lock(&self.child).inner_mut().try_wait(), Ok(None))
    }

    /// After the leader exited: terminate the job, so stragglers die. True
    /// when the termination was proven.
    pub(crate) fn reap(&self) -> bool {
        let mut child = lock(&self.child);
        child.start_kill().is_ok() && child.wait().is_ok()
    }
}

/// Spawn `argv` inside a fresh kill-on-close job.
///
/// # Errors
///
/// The OS error of the pipe, the spawn, or the job assignment (nothing runs
/// outside a job: a failed assignment terminates the suspended child).
pub(crate) fn spawn(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> std::io::Result<Spawned> {
    let (output, output_write) = std::io::pipe()?;
    let Some((program, arguments)) = argv.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty argv",
        ));
    };
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .env("NoDefaultCurrentDirectoryInExePath", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output_write.try_clone()?))
        .stderr(Stdio::from(output_write));
    let mut wrapped = CommandWrap::from(command);
    wrapped.wrap(JobObject);
    let child = wrapped.spawn()?;
    drop(wrapped);
    let pid = child.id();
    Ok(Spawned {
        process: Process {
            pid,
            child: Arc::new(Mutex::new(child)),
        },
        channel: ControlChannel,
        output,
    })
}

/// Bytes waiting unread: not observable on an anonymous pipe here, so the
/// drain keeps its quiescence heuristic.
pub(crate) fn pending_bytes(_output: &PipeReader) -> bool {
    false
}

/// The pump's reads block until data or EOF.
pub(crate) fn wait_readable(_output: &PipeReader) -> bool {
    true
}
