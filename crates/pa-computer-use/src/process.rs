//! The process environment seam: subprocesses, `PATH` lookup, files and
//! environment variables. Backends that drive tools (`xdotool`, `xwininfo`,
//! `maim`/`scrot`, `grim`, `loginctl`, `screencapture`, `mdfind`, `open`)
//! reach the system only through [`Tools`], so tests script every run.

use std::path::Path;
use std::time::Duration;

use crate::error::{ComputerUseError, ERROR_LIMIT, head, transport};

/// One finished run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CommandOutput {
    /// The exit code; a signal-killed run reports the negated signal.
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub(crate) fn success(&self) -> bool {
        self.code == 0
    }

    /// The stderr (else stdout), decoded leniently, trimmed and capped for
    /// an error message.
    pub(crate) fn capped(&self) -> String {
        let bytes = if self.stderr.is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        head(String::from_utf8_lossy(bytes).trim(), ERROR_LIMIT).to_string()
    }
}

/// Why a run produced no output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunError {
    TimedOut,
    /// The tool could not be started: the OS error text.
    Unavailable(String),
}

/// The process environment, faked in tests.
pub(crate) trait Tools: Send + Sync {
    /// Run `argv` to completion, capturing its output, killed at `timeout`.
    fn run(&self, argv: &[String], timeout: Duration) -> Result<CommandOutput, RunError>;
    /// Python's `shutil.which(name)`.
    fn which(&self, name: &str) -> Option<String>;
    fn is_file(&self, path: &str) -> bool;
    /// Whether `path` (followed) is a socket.
    fn is_socket(&self, path: &str) -> bool;
    fn env(&self, key: &str) -> Option<String>;
}

/// The tool timeout every backend uses.
pub(crate) const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// Run one tool, mapping an unrunnable tool or a timeout to `TRANSPORT_ERROR`
/// naming the tool.
pub(crate) fn run_tool(
    tools: &dyn Tools,
    argv: &[String],
    timeout: Duration,
) -> Result<CommandOutput, ComputerUseError> {
    let name = Path::new(&argv[0]).file_name().map_or_else(
        || argv[0].clone(),
        |name| name.to_string_lossy().into_owned(),
    );
    tools.run(argv, timeout).map_err(|error| match error {
        RunError::TimedOut => transport(format!(
            "{name} timed out after {} seconds",
            timeout.as_secs()
        )),
        RunError::Unavailable(reason) => transport(format!(
            "{name} is not available: {}",
            head(&reason, ERROR_LIMIT)
        )),
    })
}

/// Resolve one tool: its absolute candidate when that file exists, else `PATH`.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))] // the Linux backends' tools
pub(crate) fn optional_tool(
    tools: &dyn Tools,
    name: &str,
    absolute: Option<&str>,
) -> Option<String> {
    if let Some(absolute) = absolute.filter(|path| tools.is_file(path)) {
        return Some(absolute.to_string());
    }
    tools.which(name)
}

/// The host's real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SystemTools;

impl Tools for SystemTools {
    fn run(&self, argv: &[String], timeout: Duration) -> Result<CommandOutput, RunError> {
        use std::io::Read;
        use std::process::{Command, Stdio};

        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| RunError::Unavailable(error.to_string()))?;
        let reader = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_end(&mut bytes);
                }
                bytes
            })
        };
        let stdout = reader(
            child
                .stdout
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        );
        let stderr = reader(
            child
                .stderr
                .take()
                .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        );
        let deadline = std::time::Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RunError::TimedOut);
                }
                // The child is still running: poll again shortly (std has
                // no timed wait; the pipes drain on their own threads).
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => return Err(RunError::Unavailable(error.to_string())),
            }
        };
        Ok(CommandOutput {
            code: exit_code(status),
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        })
    }

    fn which(&self, name: &str) -> Option<String> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).find_map(|dir| {
            let candidate = dir.join(name);
            is_executable(&candidate).then(|| candidate.to_string_lossy().into_owned())
        })
    }

    fn is_file(&self, path: &str) -> bool {
        Path::new(path).is_file()
    }

    fn is_socket(&self, path: &str) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            false
        }
    }

    fn env(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map_or(-1, |signal| -signal)
    }
    #[cfg(not(unix))]
    {
        -1
    }
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
pub(crate) mod script;

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_real_run_captures_output_and_the_exit_code() {
        let output = SystemTools
            .run(
                &argv(&["sh", "-c", "printf out; printf err >&2; exit 3"]),
                TOOL_TIMEOUT,
            )
            .unwrap();
        assert_eq!(
            output,
            CommandOutput {
                code: 3,
                stdout: b"out".to_vec(),
                stderr: b"err".to_vec()
            }
        );
        assert_eq!(output.capped(), "err");
    }

    #[test]
    fn a_hung_run_is_killed_at_its_timeout() {
        let started = std::time::Instant::now();
        let result = SystemTools.run(&argv(&["sleep", "30"]), Duration::from_millis(100));
        assert_eq!(result, Err(RunError::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_missing_tool_maps_to_a_transport_error_naming_it() {
        let error =
            run_tool(&SystemTools, &argv(&["/nonexistent/xdotool"]), TOOL_TIMEOUT).unwrap_err();
        assert_eq!(error.code, crate::error::ErrorCode::TransportError);
        assert!(
            error.message.starts_with("xdotool is not available: "),
            "{}",
            error.message
        );
    }
}
