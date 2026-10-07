//! A stdio MCP server process: spawned in its own process group with the
//! environment it is configured to see, its stderr drained into a bounded
//! tail, and reaped as a tree (gracefully first) when its connection closes
//! or is dropped.

use std::time::Duration;

use super::connect::StdioLaunch;
use super::diagnostic::StderrTail;
use super::error::{McpErrorKind, McpSessionError};
use crate::platform::process as platform_process;

/// How long a server may take to exit once its stdin closes before its
/// process tree is killed.
const GRACEFUL_EXIT: Duration = Duration::from_secs(2);

pub(crate) struct StdioChild {
    child: tokio::process::Child,
    pid: Option<u32>,
    pub(crate) stderr: StderrTail,
    reaped: bool,
}

impl StdioChild {
    /// Spawn `launch`; returns the child and its stdout/stdin pipes.
    ///
    /// # Errors
    ///
    /// `FileNotFoundError` / `PermissionError` / `OSError` for a command
    /// that cannot start.
    pub(crate) fn spawn(
        launch: &StdioLaunch,
    ) -> Result<
        (
            Self,
            tokio::process::ChildStdout,
            tokio::process::ChildStdin,
        ),
        McpSessionError,
    > {
        let mut command = std::process::Command::new(&launch.command);
        command
            .args(&launch.args)
            .current_dir(&launch.cwd)
            .env_clear()
            .envs(launch.env.iter().map(|(key, value)| (key, value)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Its own group: closing the connection reaps everything it spawned.
        platform_process::set_new_process_group(&mut command);
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| spawn_error(&launch.command, &error))?;
        let pid = child.id();
        let stderr = StderrTail::default();
        if let Some(pipe) = child.stderr.take() {
            stderr.drain(pipe);
        }
        let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
            return Err(McpSessionError::runtime("MCP stdio pipes are unavailable"));
        };
        Ok((
            Self {
                child,
                pid,
                stderr,
                reaped: false,
            },
            stdout,
            stdin,
        ))
    }

    /// Wait (bounded) for the server to exit on its own (its stdin is
    /// closed), then kill whatever is left of its process tree.
    pub(crate) async fn shutdown(mut self) {
        if tokio::time::timeout(GRACEFUL_EXIT, self.child.wait())
            .await
            .is_err()
        {
            self.kill_tree();
            let _ = self.child.kill().await;
        }
        // Descendants the leader left behind in its group.
        if let Some(pid) = self.pid {
            let _ =
                platform_process::signal_process_group(pid as i32, platform_process::Signal::Kill);
        }
        self.reaped = true;
    }

    fn kill_tree(&self) {
        if let Some(pid) = self.pid {
            let _ = platform_process::kill_process_group_or_pid(pid as i32);
        }
    }

    /// True once the server process exited.
    pub(crate) fn has_exited(&mut self) -> bool {
        !matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for StdioChild {
    fn drop(&mut self) {
        if !self.reaped {
            self.kill_tree();
        }
    }
}

/// The in-kernel client's `OSError` for a command that cannot start
/// (`[Errno 2] No such file or directory: 'cmd'`).
fn spawn_error(command: &str, error: &std::io::Error) -> McpSessionError {
    let kind = match error.kind() {
        std::io::ErrorKind::NotFound => McpErrorKind::FileNotFound,
        std::io::ErrorKind::PermissionDenied => McpErrorKind::Permission,
        _ => McpErrorKind::Os,
    };
    let message = match error.raw_os_error() {
        Some(code) => {
            let text = error.to_string();
            let description = text
                .strip_suffix(&format!(" (os error {code})"))
                .unwrap_or(&text);
            format!("[Errno {code}] {description}: '{command}'")
        }
        None => error.to_string(),
    };
    McpSessionError::new(kind, message)
}
