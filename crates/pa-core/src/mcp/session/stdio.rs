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
        #[cfg(windows)]
        let program = resolve_windows_command(
            &launch.command,
            launch
                .env
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
                .map(|(_, value)| value.as_str()),
            std::env::var("PATHEXT").ok().as_deref(),
            &launch.cwd,
            std::path::Path::is_file,
        );
        #[cfg(not(windows))]
        let program = launch.command.clone();
        // A resolved `.cmd`/`.bat` runs through `cmd.exe /c`: std's Windows
        // spawn does that itself, quoting each argument for cmd (and refusing
        // one it cannot quote safely).
        let mut command = std::process::Command::new(&program);
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

/// Script extensions tried after `PATHEXT` (the Python SDK's fallback list,
/// minus `.ps1`, which no process spawn can run directly).
#[cfg(any(windows, test))]
const WINDOWS_SCRIPT_FALLBACKS: [&str; 3] = [".cmd", ".bat", ".exe"];

/// Resolve a stdio server command to the file Windows can spawn, as the
/// in-kernel client (the Python SDK's `get_windows_executable_command`) did:
/// `npx` becomes `C:\...\npx.cmd`. The `PATHEXT` candidates are searched
/// across `path` (the server's own `PATH`, `;`-separated) first, then the
/// script fallbacks; an extensionless file never matches (npm installs a
/// POSIX `npx` script next to `npx.cmd`). A command with a directory part is
/// probed where it points (relative to `cwd`, returned joined so the
/// spawn does not re-resolve it against the host's own directory). The current directory is not
/// searched: a planted executable there must not win. Unresolved, the
/// command is returned as given.
#[cfg(any(windows, test))]
fn resolve_windows_command(
    command: &str,
    path: Option<&str>,
    pathext: Option<&str>,
    cwd: &std::path::Path,
    is_file: impl Fn(&std::path::Path) -> bool,
) -> String {
    let has_extension = |name: &str| std::path::Path::new(name).extension().is_some();
    // Pass one is the SDK's `shutil.which(command)` (PATHEXT); pass two its
    // per-extension fallbacks. Each pass walks `path` in order.
    let pathext: Vec<String> = platform_process::windows_executable_candidates(command, pathext)
        .into_iter()
        .filter(|candidate| has_extension(candidate))
        .collect();
    let fallbacks: Vec<Vec<String>> = WINDOWS_SCRIPT_FALLBACKS
        .iter()
        .map(|extension| vec![format!("{command}{extension}")])
        .collect();
    let passes = std::iter::once(pathext).chain(fallbacks);
    if command.contains(['\\', '/', ':']) {
        return passes
            .flatten()
            .map(|candidate| cwd.join(candidate))
            .find(|candidate| is_file(candidate))
            .map_or_else(
                || command.to_string(),
                |candidate| candidate.to_string_lossy().into_owned(),
            );
    }
    let dirs: Vec<&str> = path
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .collect();
    for pass in passes {
        for dir in &dirs {
            for candidate in &pass {
                let full = std::path::Path::new(dir).join(candidate);
                if is_file(&full) {
                    return full.to_string_lossy().into_owned();
                }
            }
        }
    }
    command.to_string()
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    use super::resolve_windows_command;

    /// Resolve `command` over a fake filesystem holding `files`.
    fn resolve(command: &str, path: &str, pathext: Option<&str>, files: &[&str]) -> String {
        let files: HashSet<PathBuf> = files.iter().map(PathBuf::from).collect();
        resolve_windows_command(
            command,
            Some(path),
            pathext,
            Path::new("/work"),
            |candidate| files.contains(candidate),
        )
    }

    const PATHEXT: Option<&str> = Some(".COM;.EXE;.BAT;.CMD;.VBS;.PS1");

    #[test]
    fn a_bare_npm_shim_resolves_to_its_cmd_file_never_the_posix_script() {
        assert_eq!(
            resolve(
                "npx",
                "/node;/other",
                PATHEXT,
                &["/node/npx", "/node/npx.cmd"]
            ),
            "/node/npx.cmd"
        );
    }

    #[test]
    fn pathext_order_wins_within_a_directory_and_path_order_across_them() {
        assert_eq!(
            [
                resolve(
                    "tool",
                    "/a",
                    Some(".CMD;.EXE"),
                    &["/a/tool.exe", "/a/tool.cmd"]
                ),
                resolve(
                    "tool",
                    "/a",
                    Some(".EXE;.CMD"),
                    &["/a/tool.exe", "/a/tool.cmd"]
                ),
                resolve("tool", "/a;/b", PATHEXT, &["/a/tool.cmd", "/b/tool.exe"]),
            ],
            ["/a/tool.cmd", "/a/tool.exe", "/a/tool.cmd"]
        );
    }

    #[test]
    fn the_script_fallbacks_apply_when_pathext_omits_them() {
        assert_eq!(
            [
                resolve("tool", "/a", Some(".EXE"), &["/a/tool.cmd"]),
                resolve("tool", "/a", None, &["/a/tool.bat"]),
            ],
            ["/a/tool.cmd", "/a/tool.bat"]
        );
    }

    #[test]
    fn an_explicit_extension_and_a_directory_part_are_kept() {
        assert_eq!(
            [
                resolve("npx.cmd", "/node", PATHEXT, &["/node/npx.cmd"]),
                resolve("bin/serve", "/node", PATHEXT, &["/work/bin/serve.bat"]),
                resolve("/opt/serve", "", PATHEXT, &["/opt/serve.exe"]),
            ],
            ["/node/npx.cmd", "/work/bin/serve.bat", "/opt/serve.exe"]
        );
    }

    #[test]
    fn an_unresolved_command_passes_through_and_the_cwd_is_not_searched() {
        assert_eq!(
            [
                resolve("missing", "/a", PATHEXT, &[]),
                resolve("npx", "", PATHEXT, &["/work/npx.cmd"]),
                resolve("python", "/a", PATHEXT, &["/a/python"]),
            ],
            ["missing", "npx", "python"]
        );
    }
}
