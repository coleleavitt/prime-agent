//! Read-only shell probes the guards run before deciding (`git status`, the
//! upstream of the current branch). A probe runs in the kernel's shell and
//! environment, in its own process group, with a bounded wait and a bounded
//! output read: a wedged git, or a descendant that keeps the output pipe open,
//! is killed with its whole group instead of wedging the check.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::context::GuardContext;
use crate::shell::{child_env, resolve_shell};

/// How long a probe may run and how much of its output is read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProbeLimits {
    /// The wait for the probe's output to end.
    pub timeout: Duration,
    /// After a timeout (or a capped read), how long to wait for the reader
    /// to notice the killed group closed the pipe.
    pub kill_grace: Duration,
    /// Read at most this many bytes; past it the probe's group is killed and
    /// the bytes read so far are reported as truncated.
    pub output_cap: Option<usize>,
}

/// How a probe ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// The probe's output ended. `status` is its exit code (`None` when it
    /// died by a signal or could not be waited for); `truncated` marks output
    /// cut at the cap (the probe was then killed, so its status is moot).
    Finished {
        status: Option<i32>,
        stdout: Vec<u8>,
        truncated: bool,
    },
    /// The output did not end inside the timeout; the group was killed.
    TimedOut,
    /// No shell could be chosen or the process could not start.
    Unavailable,
}

/// Run `script` with the kernel's shell in `cwd`.
pub(crate) fn run_probe(
    context: &GuardContext,
    script: &str,
    cwd: &Path,
    limits: ProbeLimits,
) -> ProbeOutcome {
    let Ok(shell) = resolve_shell(context) else {
        return ProbeOutcome::Unavailable;
    };
    let Ok(mut command) = context.sandbox().command(shell) else {
        return ProbeOutcome::Unavailable;
    };
    command
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .env_clear()
        .envs(child_env(context))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    new_process_group(&mut command);
    let Ok(mut child) = command.spawn() else {
        return ProbeOutcome::Unavailable;
    };
    let Some(mut stdout) = child.stdout.take() else {
        kill_group(&mut child);
        return ProbeOutcome::Unavailable;
    };
    let group = child.id();
    let cap = limits.output_cap;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut data = Vec::new();
        let read = match cap {
            Some(cap) => (&mut stdout)
                .take(u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1))
                .read_to_end(&mut data),
            None => stdout.read_to_end(&mut data),
        };
        if read.is_err() {
            data.clear();
        }
        let truncated = cap.is_some_and(|cap| data.len() > cap);
        if truncated {
            // The listing already says enough: do not wait for the rest.
            kill_group_id(group);
        }
        let _ = sender.send((data, truncated));
    });
    let delivered = receiver.recv_timeout(limits.timeout).ok();
    if delivered.is_none() {
        // A wedged probe, or a descendant still holding the pipe: kill the
        // group (which closes the pipe) and give the reader a moment.
        kill_group(&mut child);
        let _ = receiver.recv_timeout(limits.kill_grace);
    }
    let Some((mut stdout, truncated)) = delivered else {
        kill_group(&mut child);
        let _ = child.wait();
        return ProbeOutcome::TimedOut;
    };
    let status = wait_bounded(&mut child, limits.kill_grace);
    if let Some(cap) = cap {
        stdout.truncate(cap);
    }
    ProbeOutcome::Finished {
        status,
        stdout,
        truncated,
    }
}

/// Wait for the probe's exit, killing its group if it outlives `grace`.
fn wait_bounded(child: &mut Child, grace: Duration) -> Option<i32> {
    let deadline = std::time::Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) | Err(_) => {
                kill_group(child);
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(unix)]
fn new_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn new_process_group(_command: &mut Command) {}

fn kill_group(child: &mut Child) {
    kill_group_id(child.id());
    let _ = child.kill();
}

#[cfg(unix)]
fn kill_group_id(group: u32) {
    if let Some(pid) = i32::try_from(group)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(not(unix))]
fn kill_group_id(_group: u32) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn context() -> GuardContext {
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
        GuardContext::new("/", env)
    }

    const LIMITS: ProbeLimits = ProbeLimits {
        timeout: Duration::from_secs(10),
        kill_grace: Duration::from_secs(1),
        output_cap: Some(8),
    };

    #[test]
    fn a_finished_probe_reports_its_status_and_capped_output() {
        let outcome = run_probe(&context(), "printf abc; exit 3", Path::new("/"), LIMITS);
        assert_eq!(
            outcome,
            ProbeOutcome::Finished {
                status: Some(3),
                stdout: b"abc".to_vec(),
                truncated: false
            }
        );
        let outcome = run_probe(
            &context(),
            "printf 0123456789; sleep 30",
            Path::new("/"),
            LIMITS,
        );
        assert!(
            matches!(&outcome, ProbeOutcome::Finished { stdout, truncated: true, .. } if stdout == b"01234567"),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_descendant_holding_the_pipe_times_out_and_is_killed() {
        let limits = ProbeLimits {
            timeout: Duration::from_millis(200),
            ..LIMITS
        };
        let outcome = run_probe(&context(), "(sleep 30) & exit 0", Path::new("/"), limits);
        assert_eq!(outcome, ProbeOutcome::TimedOut);
    }
}
