//! The replay runner (TS `ravo/referee-runner.ts` `runReplayCase` and
//! `ravo/python-environment.ts`): a replay case runs in the kernel's
//! Python in isolated mode (`-I -B`, the program on stdin), in a fresh
//! temporary working directory removed afterwards, with an environment of
//! `PATH`, `HOME`, `LANG` (and on Windows what `CPython` needs to start) plus
//! explicit `sys.path` roots. The interpreter leads its own process group,
//! which is killed when the run ends and journaled as a possible orphan
//! while it runs, so supervisor recovery reaps it after a worker dies.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pa_ledger::ReplayCase;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::referee::{ReplayEnvironment, ReplayOutcome, ReplayRunner};

/// How long one replay may run.
pub const DEFAULT_REPLAY_TIMEOUT: Duration = Duration::from_secs(10);

const MAX_DETAIL_UNITS: usize = 1000;
const MAX_CAPTURE_BYTES: usize = MAX_DETAIL_UNITS * 2;
const RAISED_MARKER: &str = "RAISED ";
const CLEAN_MARKER: &str = "CLEAN";

/// The host variables every replay environment keeps.
const SANITIZED_ENV_KEYS: [&str; 3] = ["PATH", "HOME", "LANG"];

/// On Windows also what `CPython` needs to start and open sockets.
#[cfg(windows)]
const WINDOWS_ENV_KEYS: [&str; 7] = [
    "SYSTEMROOT",
    "WINDIR",
    "USERPROFILE",
    "TEMP",
    "TMP",
    "COMSPEC",
    "PATHEXT",
];

/// Runs replay cases in a Python subprocess.
#[derive(Debug, Clone)]
pub struct PythonReplayRunner {
    /// The interpreter; `None` resolves the kernel's at each run.
    pub python: Option<PathBuf>,
    pub timeout: Duration,
}

impl Default for PythonReplayRunner {
    fn default() -> Self {
        Self {
            python: None,
            timeout: DEFAULT_REPLAY_TIMEOUT,
        }
    }
}

/// The wrapper program: re-applies `PYTHONPATH` (isolated mode ignores it)
/// and reports how the case ended.
fn replay_program(source: &str) -> String {
    [
        "import os, sys".to_string(),
        "sys.path[0:0] = [p for p in os.environ.get('PYTHONPATH', '').split(os.pathsep) if p]"
            .to_string(),
        format!("source = {}", crate::js::json_string(source)),
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

/// `text.trim()`, capped at 1 000 UTF-16 units with an ellipsis.
fn trim_detail(text: &str) -> String {
    let trimmed = text.trim();
    let mut used = 0;
    for (index, ch) in trimmed.char_indices() {
        used += ch.len_utf16();
        if used > MAX_DETAIL_UNITS {
            return format!("{}\u{2026}", &trimmed[..index]);
        }
    }
    trimmed.to_string()
}

/// How a finished interpreter's output reads.
fn parse_outcome(
    stdout: &str,
    stderr: &str,
    code: Option<i32>,
    signal: Option<i32>,
) -> ReplayOutcome {
    if let Some(signal) = signal {
        return ReplayOutcome::Unrunnable {
            detail: format!("replay case killed by {}", signal_name(signal)),
        };
    }
    if code == Some(0) && stdout.trim() == CLEAN_MARKER {
        return ReplayOutcome::Clean {
            detail: "replay case completed without raising".to_string(),
        };
    }
    if code == Some(3) {
        if let Some(body) = stdout.strip_prefix(RAISED_MARKER) {
            let (class, message) = body.split_once('\n').unwrap_or((body, ""));
            let exception_class = class.trim().to_string();
            let message = message.trim();
            if !exception_class.is_empty() {
                let detail = if message.is_empty() {
                    trim_detail(&exception_class)
                } else {
                    trim_detail(&format!("{exception_class}: {message}"))
                };
                return ReplayOutcome::Raised {
                    exception_class,
                    detail,
                };
            }
        }
    }
    let reported = [trim_detail(stderr), trim_detail(stdout)]
        .into_iter()
        .find(|text| !text.is_empty());
    ReplayOutcome::Unrunnable {
        detail: reported.unwrap_or_else(|| {
            format!(
                "replay case exited {} without a verdict",
                code.map_or_else(|| "unknown".to_string(), |code| code.to_string())
            )
        }),
    }
}

fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP".to_string(),
        2 => "SIGINT".to_string(),
        3 => "SIGQUIT".to_string(),
        6 => "SIGABRT".to_string(),
        9 => "SIGKILL".to_string(),
        11 => "SIGSEGV".to_string(),
        13 => "SIGPIPE".to_string(),
        15 => "SIGTERM".to_string(),
        other => format!("signal {other}"),
    }
}

/// The run's environment: the kept host variables and `PYTHONPATH` of the
/// roots, resolved against this process's working directory.
fn replay_environment(
    environment: ReplayEnvironment,
    sys_path: &[String],
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut keep = |key: &str| {
        if let Ok(value) = std::env::var(key) {
            env.push((key.to_string(), value));
        }
    };
    for key in SANITIZED_ENV_KEYS {
        keep(key);
    }
    #[cfg(windows)]
    for key in WINDOWS_ENV_KEYS {
        keep(key);
    }
    let mut roots: Vec<PathBuf> = sys_path
        .iter()
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .collect();
    if environment == ReplayEnvironment::SkillImport {
        if let Some(host) = std::env::var_os("PYTHONPATH") {
            roots.extend(std::env::split_paths(&host).filter(|root| !root.as_os_str().is_empty()));
        }
    }
    let mut resolved: Vec<PathBuf> = Vec::new();
    for root in roots {
        let absolute = std::path::absolute(&root).unwrap_or(root);
        if !resolved.contains(&absolute) {
            resolved.push(absolute);
        }
    }
    if let Ok(joined) = std::env::join_paths(&resolved) {
        if !resolved.is_empty() {
            env.push((
                "PYTHONPATH".to_string(),
                joined.to_string_lossy().into_owned(),
            ));
        }
    }
    env
}

/// A fresh, unique temporary working directory.
fn make_workdir() -> std::io::Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let dir = std::env::temp_dir().join(format!(
        "prime-agent-replay-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir)?;
    Ok(dir)
}

async fn read_capped(mut reader: impl tokio::io::AsyncRead + Unpin) -> String {
    let mut captured: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if captured.len() < MAX_CAPTURE_BYTES {
                    captured.extend_from_slice(&chunk[..read]);
                }
            }
        }
    }
    captured.truncate(MAX_CAPTURE_BYTES);
    String::from_utf8_lossy(&captured).into_owned()
}

#[cfg(unix)]
fn exit_signal(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: std::process::ExitStatus) -> Option<i32> {
    None
}

impl PythonReplayRunner {
    async fn run_in(
        &self,
        python: &Path,
        case: &ReplayCase,
        workdir: &Path,
        env: Vec<(String, String)>,
    ) -> ReplayOutcome {
        let mut command = std::process::Command::new(python);
        command
            .args(["-I", "-B", "-"])
            .current_dir(workdir)
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        pa_core::platform::set_new_process_group(&mut command);
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        // The kernel venv python can be mid-rewrite (ETXTBSY): ride it out.
        let mut child =
            match pa_core::platform::process::spawn_retrying_text_busy(&mut command).await {
                Ok(child) => child,
                Err(error) => {
                    return ReplayOutcome::Unrunnable {
                        detail: format!("spawn failed for {}: {error}", python.display()),
                    };
                }
            };
        let pid = child.id().and_then(|pid| i32::try_from(pid).ok());
        if let Some(pid) = pid {
            pa_core::kernel::orphan_journal::record_orphan_process_state(pid, true);
        }
        let program = replay_program(&case.source);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let run = async {
            if let Some(mut stdin) = stdin {
                // The interpreter may exit before reading its program; the
                // exit status reports why.
                let _ = stdin.write_all(program.as_bytes()).await;
            }
            let (out, err, status) = tokio::join!(
                async {
                    match stdout {
                        Some(stdout) => read_capped(stdout).await,
                        None => String::new(),
                    }
                },
                async {
                    match stderr {
                        Some(stderr) => read_capped(stderr).await,
                        None => String::new(),
                    }
                },
                child.wait()
            );
            (out, err, status)
        };
        let outcome = match tokio::time::timeout(self.timeout, run).await {
            Ok((out, err, Ok(status))) => {
                parse_outcome(&out, &err, status.code(), exit_signal(status))
            }
            Ok((_, _, Err(error))) => ReplayOutcome::Unrunnable {
                detail: format!("spawn failed for {}: {error}", python.display()),
            },
            Err(_) => ReplayOutcome::Unrunnable {
                detail: format!("replay case timed out after {}ms", self.timeout.as_millis()),
            },
        };
        if let Some(pid) = pid {
            // The whole group goes, so no grandchild outlives the run.
            let _ = pa_core::platform::kill_process_group_or_pid(pid);
            pa_core::kernel::orphan_journal::record_orphan_process_state(pid, false);
        }
        outcome
    }
}

impl ReplayRunner for PythonReplayRunner {
    fn run<'a>(
        &'a self,
        case: &'a ReplayCase,
        environment: ReplayEnvironment,
        sys_path: &'a [String],
    ) -> Pin<Box<dyn Future<Output = ReplayOutcome> + Send + 'a>> {
        Box::pin(async move {
            let python = self
                .python
                .clone()
                .or_else(pa_core::kernel::bootstrap::installed_kernel_python);
            let Some(python) = python else {
                return ReplayOutcome::Unrunnable {
                    detail: "no kernel python: the replay case could not be executed".to_string(),
                };
            };
            let mut roots: Vec<String> = sys_path.to_vec();
            roots.extend(case.sys_path.iter().flatten().cloned());
            let env = replay_environment(environment, &roots);
            let workdir = match make_workdir() {
                Ok(workdir) => workdir,
                Err(error) => {
                    return ReplayOutcome::Unrunnable {
                        detail: format!("no working directory for the replay case: {error}"),
                    };
                }
            };
            let outcome = self.run_in(&python, case, &workdir, env).await;
            let _ = std::fs::remove_dir_all(&workdir);
            outcome
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_read_like_the_ts_runner() {
        assert_eq!(
            parse_outcome("CLEAN", "", Some(0), None),
            ReplayOutcome::Clean {
                detail: "replay case completed without raising".to_string()
            }
        );
        assert_eq!(
            parse_outcome(
                "RAISED ModuleNotFoundError\nNo module named 'x'",
                "",
                Some(3),
                None
            ),
            ReplayOutcome::Raised {
                exception_class: "ModuleNotFoundError".to_string(),
                detail: "ModuleNotFoundError: No module named 'x'".to_string()
            }
        );
        assert_eq!(
            parse_outcome("", "boom\n", Some(1), None),
            ReplayOutcome::Unrunnable {
                detail: "boom".to_string()
            }
        );
        assert_eq!(
            parse_outcome("", "", Some(1), None),
            ReplayOutcome::Unrunnable {
                detail: "replay case exited 1 without a verdict".to_string()
            }
        );
        assert_eq!(
            parse_outcome("", "", None, Some(9)),
            ReplayOutcome::Unrunnable {
                detail: "replay case killed by SIGKILL".to_string()
            }
        );
    }

    #[test]
    fn the_wrapper_program_is_the_ts_one() {
        assert_eq!(
            replay_program("import x"),
            "import os, sys\nsys.path[0:0] = [p for p in os.environ.get('PYTHONPATH', '').split(os.pathsep) if p]\nsource = \"import x\"\ntry:\n    exec(compile(source, '<replay-case>', 'exec'), {'__name__': '__replay__'})\nexcept BaseException as exc:\n    sys.stdout.write('RAISED ' + type(exc).__name__ + '\\n' + str(exc)[:1000])\n    sys.stdout.flush()\n    raise SystemExit(3)\nsys.stdout.write('CLEAN')\n"
        );
    }

    /// A missing interpreter is unrunnable, never a pass.
    #[tokio::test]
    async fn a_missing_interpreter_is_unrunnable() {
        let runner = PythonReplayRunner {
            python: Some(PathBuf::from("/nonexistent/prime-agent-python")),
            timeout: Duration::from_secs(5),
        };
        let case = ReplayCase {
            language: "python".to_string(),
            source: "import json".to_string(),
            exception_class: None,
            sys_path: None,
            verified_at: None,
        };
        let outcome = runner.run(&case, ReplayEnvironment::Sanitized, &[]).await;
        assert!(
            matches!(&outcome, ReplayOutcome::Unrunnable { detail } if detail.starts_with("spawn failed for /nonexistent/prime-agent-python")),
            "{outcome:?}"
        );
    }

    /// `python3` on PATH, for the tests that run a real interpreter.
    fn host_python() -> Option<PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
    }

    /// A real interpreter: a module that imports runs clean, a missing one
    /// raises its recorded class, and the temporary directory is gone.
    #[tokio::test]
    async fn a_real_interpreter_reports_clean_and_raised_runs() {
        // Needs some Python 3; every development and CI host has one.
        let Some(python) = host_python() else {
            eprintln!("no python3 on PATH; the subprocess run is not exercised");
            return;
        };
        let runner = PythonReplayRunner {
            python: Some(python),
            timeout: Duration::from_secs(30),
        };
        let case = |source: &str| ReplayCase {
            language: "python".to_string(),
            source: source.to_string(),
            exception_class: None,
            sys_path: None,
            verified_at: None,
        };
        assert_eq!(
            runner
                .run(&case("import json"), ReplayEnvironment::Sanitized, &[])
                .await,
            ReplayOutcome::Clean {
                detail: "replay case completed without raising".to_string()
            }
        );
        assert_eq!(
            runner
                .run(
                    &case("import prime_agent_replay_missing_module"),
                    ReplayEnvironment::SkillImport,
                    &[]
                )
                .await,
            ReplayOutcome::Raised {
                exception_class: "ModuleNotFoundError".to_string(),
                detail: "ModuleNotFoundError: No module named 'prime_agent_replay_missing_module'"
                    .to_string()
            }
        );
    }

    /// The kernel interpreter can be open for writing at the spawn instant
    /// (ETXTBSY: a concurrent bootstrap rewriting the venv, or a fork that
    /// still holds the write handle until its exec). The runner rides that
    /// transient window out instead of reporting the case unrunnable.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_interpreter_busy_being_written_still_runs_the_case() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let python = dir.path().join("python");
        std::fs::write(&python, "#!/bin/sh\nprintf CLEAN\n").unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&python)
            .unwrap();
        writer.write_all(b"# appended by the writer\n").unwrap();
        let release = std::thread::spawn(move || {
            // Fault injection, not a readiness wait: the concurrent
            // writer's hold lasts this long (inside the 20 x 25 ms budget).
            std::thread::sleep(Duration::from_millis(150));
            drop(writer);
        });
        let runner = PythonReplayRunner {
            python: Some(python),
            timeout: Duration::from_secs(30),
        };
        let case = ReplayCase {
            language: "python".to_string(),
            source: "import json".to_string(),
            exception_class: None,
            sys_path: None,
            verified_at: None,
        };
        let outcome = runner.run(&case, ReplayEnvironment::Sanitized, &[]).await;
        release.join().unwrap();
        assert_eq!(
            outcome,
            ReplayOutcome::Clean {
                detail: "replay case completed without raising".to_string()
            }
        );
    }
}
