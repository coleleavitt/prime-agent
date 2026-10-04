//! The subprocess half of the double-run gate: run an exit test once, in a
//! fresh interpreter, and classify how it ended.
//!
//! The exit test is capability code the model wrote to run where the user's
//! own code runs, so it inherits this process's environment (proxies,
//! certificates, the user's `PYTHONPATH`) with the package root first on the
//! path. The interpreter runs isolated (`-I -B`, so `PYTHON*` variables are
//! ignored and the wrapper re-applies `PYTHONPATH` itself), reads its program
//! from stdin so `sys.argv` carries nothing, and leads its own process group,
//! which is killed when the run ends or times out, so a grandchild cannot
//! outlive the gate. A live group is recorded in the orphan process journal
//! for the supervisor to reap if this process dies first. A run that could
//! not be performed is `unrunnable`, never a pass.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Default limit for one gate run.
pub const DEFAULT_GATE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DETAIL_CHARS: usize = 1_000;
const MAX_CAPTURE_CHARS: usize = MAX_DETAIL_CHARS * 2;
const RAISED_MARKER: &str = "RAISED ";
const CLEAN_MARKER: &str = "CLEAN";

/// How one run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    /// The program raised (the exception class leads the detail).
    Raised,
    /// The program completed without raising.
    Clean,
    /// The program could not be run to a verdict.
    Unrunnable,
}

impl OutcomeKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raised => "raised",
            Self::Clean => "clean",
            Self::Unrunnable => "unrunnable",
        }
    }
}

/// The verdict of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub kind: OutcomeKind,
    pub detail: String,
}

impl RunOutcome {
    fn unrunnable(detail: impl Into<String>) -> Self {
        Self {
            kind: OutcomeKind::Unrunnable,
            detail: detail.into(),
        }
    }
}

/// The wrapper program: re-applies `PYTHONPATH` under `-I`, runs the exit
/// test, and reports a verdict on stdout.
fn wrapper_program(source: &str) -> String {
    let literal = serde_json::to_string(source).unwrap_or_else(|_| "\"\"".to_string());
    [
        "import os, sys".to_string(),
        "sys.path[0:0] = [p for p in os.environ.get('PYTHONPATH', '').split(os.pathsep) if p]"
            .to_string(),
        format!("source = {literal}"),
        "try:".to_string(),
        "    exec(compile(source, '<replay-case>', 'exec'), {'__name__': '__replay__'})"
            .to_string(),
        "except BaseException as exc:".to_string(),
        format!(
            "    sys.stdout.write('{RAISED_MARKER}' + type(exc).__name__ + '\\n' + str(exc)[:1000])"
        ),
        "    sys.stdout.flush()".to_string(),
        "    raise SystemExit(3)".to_string(),
        format!("sys.stdout.write('{CLEAN_MARKER}')"),
        String::new(),
    ]
    .join("\n")
}

fn trim_detail(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() > MAX_DETAIL_CHARS {
        let kept: String = trimmed.chars().take(MAX_DETAIL_CHARS).collect();
        format!("{kept}…")
    } else {
        trimmed.to_string()
    }
}

/// Classify a finished run from its captured output and exit.
fn parse_outcome(
    stdout: &str,
    stderr: &str,
    code: Option<i32>,
    signal: Option<&str>,
) -> RunOutcome {
    if let Some(signal) = signal {
        return RunOutcome::unrunnable(format!("replay case killed by {signal}"));
    }
    if code == Some(0) && stdout.trim() == CLEAN_MARKER {
        return RunOutcome {
            kind: OutcomeKind::Clean,
            detail: "replay case completed without raising".to_string(),
        };
    }
    if code == Some(3) {
        if let Some(body) = stdout.strip_prefix(RAISED_MARKER) {
            let (class, message) = body.split_once('\n').unwrap_or((body, ""));
            let class = class.trim();
            let message = message.trim();
            if !class.is_empty() {
                let detail = if message.is_empty() {
                    class.to_string()
                } else {
                    format!("{class}: {message}")
                };
                return RunOutcome {
                    kind: OutcomeKind::Raised,
                    detail: trim_detail(&detail),
                };
            }
        }
    }
    let reported = Some(trim_detail(stderr))
        .filter(|detail| !detail.is_empty())
        .unwrap_or_else(|| trim_detail(stdout));
    if reported.is_empty() {
        let code = code.map_or_else(|| "unknown".to_string(), |code| code.to_string());
        RunOutcome::unrunnable(format!("replay case exited {code} without a verdict"))
    } else {
        RunOutcome::unrunnable(reported)
    }
}

/// `PYTHONPATH` for the run: the given roots, then the host's own entries,
/// each made absolute against this process's working directory, first
/// occurrence kept.
fn inherited_python_path(roots: &[PathBuf]) -> Option<std::ffi::OsString> {
    let inherited = std::env::var_os("PYTHONPATH").unwrap_or_default();
    let mut entries: Vec<PathBuf> = Vec::new();
    for root in roots
        .iter()
        .cloned()
        .chain(std::env::split_paths(&inherited))
    {
        if root.as_os_str().is_empty() {
            continue;
        }
        let absolute = std::path::absolute(&root).unwrap_or(root);
        if !entries.contains(&absolute) {
            entries.push(absolute);
        }
    }
    if entries.is_empty() {
        None
    } else {
        std::env::join_paths(entries).ok()
    }
}

/// Read a stream to its end, keeping at most [`MAX_CAPTURE_CHARS`]
/// characters; the rest is drained and dropped.
async fn capture(stream: Option<impl AsyncRead + Unpin>) -> String {
    let Some(mut stream) = stream else {
        return String::new();
    };
    // Four bytes per character at most: enough to hold the kept characters.
    let limit = MAX_CAPTURE_CHARS * 4;
    let mut kept = Vec::new();
    // On the heap: two captures run joined, and a stack buffer would make
    // every future up the publish path carry it.
    let mut chunk = vec![0_u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let room = limit.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..read.min(room)]);
            }
        }
    }
    String::from_utf8_lossy(&kept)
        .chars()
        .take(MAX_CAPTURE_CHARS)
        .collect()
}

#[cfg(unix)]
fn signal_name(status: std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|signal| {
        nix::sys::signal::Signal::try_from(signal).map_or_else(
            |_| format!("signal {signal}"),
            |name| name.as_str().to_string(),
        )
    })
}

#[cfg(not(unix))]
fn signal_name(_status: std::process::ExitStatus) -> Option<String> {
    None
}

/// Run `exit_test` once with `src_path` first on its path, in `cwd`.
pub async fn run_exit_test(
    exit_test: &str,
    src_path: &Path,
    cwd: &Path,
    python: Option<&Path>,
    timeout: Duration,
) -> RunOutcome {
    let Some(python) = python else {
        return RunOutcome::unrunnable("no kernel python: the replay case could not be executed");
    };
    let mut command = std::process::Command::new(python);
    command
        .args(["-I", "-B", "-"])
        .current_dir(cwd)
        .env_remove("PYTHONPATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(python_path) = inherited_python_path(&[src_path.to_path_buf()]) {
        command.env("PYTHONPATH", python_path);
    }
    pa_core::platform::process::set_new_process_group(&mut command);
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return RunOutcome::unrunnable(format!(
                "spawn failed for {}: {error}",
                python.display()
            ));
        }
    };
    let pid = child.id().and_then(|pid| i32::try_from(pid).ok());
    if let Some(pid) = pid {
        pa_core::kernel::orphan_journal::record_orphan_process_state(pid, true);
    }
    let program = wrapper_program(exit_test);
    let stdin = child.stdin.take();
    let feed = async move {
        if let Some(mut stdin) = stdin {
            // The interpreter may exit before reading its program; its exit
            // status reports why.
            let _ = stdin.write_all(program.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // The group goes whether the leader exited or not: a grandchild the exit
    // test left behind must not outlive the gate. Killing it as soon as the
    // leader exits also closes the pipes it inherited, so the captures end
    // with the run instead of at the timeout.
    let kill_group = move || {
        #[cfg(unix)]
        if let Some(pid) = pid {
            let _ = pa_core::platform::process::kill_process_group(pid);
        }
    };
    let leader = async {
        let status = child.wait().await;
        kill_group();
        status
    };
    let finished = tokio::time::timeout(timeout, async {
        let ((), stdout, stderr, status) =
            tokio::join!(feed, capture(stdout), capture(stderr), leader);
        (stdout, stderr, status)
    })
    .await;
    let outcome = match finished {
        Ok((stdout, stderr, Ok(status))) => {
            let signal = signal_name(status);
            parse_outcome(&stdout, &stderr, status.code(), signal.as_deref())
        }
        Ok((_, _, Err(error))) => {
            RunOutcome::unrunnable(format!("spawn failed for {}: {error}", python.display()))
        }
        Err(_) => RunOutcome::unrunnable(format!(
            "replay case timed out after {}ms",
            timeout.as_millis()
        )),
    };
    kill_group();
    if let Some(pid) = pid {
        pa_core::kernel::orphan_journal::record_orphan_process_state(pid, false);
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_classify_like_the_referee_runner() {
        assert_eq!(
            parse_outcome("CLEAN", "", Some(0), None),
            RunOutcome {
                kind: OutcomeKind::Clean,
                detail: "replay case completed without raising".to_string()
            }
        );
        assert_eq!(
            parse_outcome("RAISED AssertionError\nexpected a-b", "", Some(3), None),
            RunOutcome {
                kind: OutcomeKind::Raised,
                detail: "AssertionError: expected a-b".to_string()
            }
        );
        assert_eq!(
            parse_outcome("RAISED NotImplementedError\n", "", Some(3), None),
            RunOutcome {
                kind: OutcomeKind::Raised,
                detail: "NotImplementedError".to_string()
            }
        );
        assert_eq!(
            parse_outcome("", "", None, Some("SIGKILL")),
            RunOutcome::unrunnable("replay case killed by SIGKILL")
        );
        assert_eq!(
            parse_outcome("", "  Fatal Python error  ", Some(1), None),
            RunOutcome::unrunnable("Fatal Python error")
        );
        assert_eq!(
            parse_outcome("", "", Some(2), None),
            RunOutcome::unrunnable("replay case exited 2 without a verdict")
        );
        // A clean marker with a nonzero exit is not a pass.
        assert_eq!(
            parse_outcome("CLEAN", "", Some(1), None),
            RunOutcome::unrunnable("CLEAN")
        );
    }

    #[test]
    fn long_details_are_capped() {
        let outcome = parse_outcome("", &"e".repeat(1_500), Some(1), None);
        assert_eq!(
            outcome,
            RunOutcome::unrunnable(format!("{}…", "e".repeat(1_000)))
        );
    }

    #[tokio::test]
    async fn without_an_interpreter_the_run_is_unrunnable() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run_exit_test("pass", dir.path(), dir.path(), None, DEFAULT_GATE_TIMEOUT).await,
            RunOutcome::unrunnable("no kernel python: the replay case could not be executed")
        );
    }
}
