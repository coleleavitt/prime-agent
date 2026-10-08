// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines).
#![allow(clippy::large_futures, clippy::too_many_lines)]
// Real Landlock enforcement on the running kernel; Linux-only.
#![cfg(target_os = "linux")]

//! Plan mode on the OS sandbox: while it is on, the kernel runs under `read-only`, so a cell,
//! `bash()`, `subprocess` and a raw libc `open(O_CREAT)` through `ctypes` all fail with EACCES
//! outside the temp directory and the session's own state. Toggling restarts the kernel into
//! the new policy and keeps the namespace through the snapshot; a policy that does not change
//! keeps the kernel; a busy kernel refuses the toggle. Skips, with a message, without the
//! kernel Python, without Landlock, or without a writable directory outside `/tmp`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use pa_core::kernel::plan_guard::{PlanModeApplied, PlanModeSwitch};
use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::{ExecuteOptions, ExecuteStatus};
use pa_core::os_sandbox::{SandboxMode, SessionSandbox};
use pa_core::settings::{SandboxSettings, Settings, SettingsManager};

fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        return Some(PathBuf::from(explicit));
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidate = PathBuf::from(format!("{home}/.prime/agent/kernel-venv/bin/python"));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping the plan-mode sandbox test",
        candidate.display()
    );
    None
}

/// A fresh directory outside `/tmp` and `$TMPDIR` (the kernel's scratch stays writable in
/// plan mode, so the workspace must sit outside both).
fn outside_tmp_base() -> Option<tempfile::TempDir> {
    let base = Path::new("/var/tmp");
    let usable = !base.starts_with(std::env::temp_dir()) && !base.starts_with("/tmp");
    let dir = usable.then(|| tempfile::tempdir_in(base).ok()).flatten();
    if dir.is_none() {
        eprintln!("/var/tmp is not usable; skipping the plan-mode sandbox test");
    }
    dir
}

/// The configured sandbox for `mode`, or `None` for `off`.
fn configured(mode: SandboxMode, workspace: &Path) -> Option<SessionSandbox> {
    let settings = SettingsManager::in_memory(&Settings {
        sandbox: Some(SandboxSettings {
            mode: Some(mode.wire_name().to_string()),
            network: Some(true),
            writable_roots: None,
        }),
        ..Default::default()
    });
    SessionSandbox::resolve(&settings, None, workspace)
}

struct Fixture {
    _base: tempfile::TempDir,
    workspace: PathBuf,
    mode: PlanModeSwitch,
    provisioner: IpythonKernelProvisioner,
}

/// A provisioner over a workspace outside the temp dirs, its sandbox configured as `sandbox`
/// and plan mode starting at `plan`; `None` (after saying why) where the test cannot run.
fn fixture(sandbox: SandboxMode, plan: bool) -> Option<Fixture> {
    fixture_in(outside_tmp_base()?, sandbox, plan)
}

/// [`fixture`] with the workspace and the artifacts under `base`.
fn fixture_in(base: tempfile::TempDir, sandbox: SandboxMode, plan: bool) -> Option<Fixture> {
    let python = kernel_python()?;
    let root = base.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    let artifacts = root.join("artifacts");
    for dir in [&workspace, &artifacts] {
        std::fs::create_dir(dir).unwrap();
    }
    let probe = SessionSandbox::for_plan_mode(None);
    if probe.status_label().ends_with("(unavailable)") {
        eprintln!(
            "skipping the plan-mode sandbox test: {}",
            probe.prompt_line()
        );
        return None;
    }
    let configured = configured(sandbox, &workspace);
    let mode = PlanModeSwitch::new(plan);
    let provisioner = IpythonKernelProvisioner::new(
        &workspace,
        IpythonKernelProvisionerOptions {
            python: Some(python),
            snapshot_dir: Some(artifacts),
            plan_mode: Some(pa_core::kernel::plan_guard::PlanMode::resolve(
                mode.clone(),
                configured.as_ref(),
            )),
            sandbox: configured,
            ..Default::default()
        },
    );
    Some(Fixture {
        _base: base,
        workspace,
        mode,
        provisioner,
    })
}

/// Run one cell; its `result` text.
async fn cell(fixture: &Fixture, code: &str) -> String {
    let manager = fixture.provisioner.ensure(None, None).await.unwrap();
    let result = manager
        .execute(code, ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(result.status, ExecuteStatus::Ok, "{result:?}");
    result.result.unwrap_or_default()
}

fn kernel_pid(fixture: &Fixture) -> Option<i32> {
    fixture
        .provisioner
        .manager()
        .and_then(|manager| manager.process_id())
}

/// A cell that tries `open(path, 'w')`: `ok`, the errno it failed with, or the exception
/// class of any other refusal.
fn python_write(path: &Path) -> String {
    format!(
        "def _probe(path):\n    try:\n        with open(path, 'w') as f:\n            f.write('x')\n        return 'ok'\n    except OSError as error:\n        return f'errno {{error.errno}}'\n    except Exception as error:\n        return type(error).__name__\n_probe({:?})",
        path.display().to_string()
    )
}

/// A cell that creates `path` with libc's `open(O_WRONLY | O_CREAT)` through `ctypes`, below
/// every Python-level hook: `created` or the errno.
fn ctypes_write(path: &Path) -> String {
    format!(
        "import ctypes, os\n_libc = ctypes.CDLL(None, use_errno=True)\n_libc.open.argtypes = [ctypes.c_char_p, ctypes.c_int, ctypes.c_int]\n_fd = _libc.open({:?}.encode(), os.O_WRONLY | os.O_CREAT, 0o644)\n'created' if _fd >= 0 and os.close(_fd) is None else f'errno {{ctypes.get_errno()}}'",
        path.display().to_string()
    )
}

/// A cell that runs `printf x > path` through `bash()`: the exit code and whether the shell
/// reported `Permission denied` (or the exception class of a refusal).
fn bash_write(path: &Path) -> String {
    format!(
        "try:\n    _r = await bash(\"printf x > '{}'\")\n    _out = (_r.exit_code, 'Permission denied' in _r.output)\nexcept Exception as error:\n    _out = type(error).__name__\n_out",
        path.display()
    )
}

/// A cell that runs `printf x > path` through `subprocess`: the return code (or the exception
/// class of a refusal).
fn subprocess_write(path: &Path) -> String {
    format!(
        "import subprocess\ntry:\n    _out = subprocess.run(['/bin/sh', '-c', \"printf x > '{}'\"], capture_output=True).returncode\nexcept Exception as error:\n    _out = type(error).__name__\n_out",
        path.display()
    )
}

const TEMP_WRITE: &str =
    "import tempfile\nwith tempfile.NamedTemporaryFile('w') as f:\n    f.write('x')\n'tmp ok'";

#[tokio::test]
async fn plan_mode_refuses_every_write_path_with_eacces_and_keeps_temp_writable() {
    let Some(fixture) = fixture(SandboxMode::Off, true) else {
        return;
    };
    let ws = &fixture.workspace;
    let results = [
        cell(&fixture, &python_write(&ws.join("cell.txt"))).await,
        cell(&fixture, &ctypes_write(&ws.join("ctypes.txt"))).await,
        cell(&fixture, &bash_write(&ws.join("bash.txt"))).await,
        cell(&fixture, &subprocess_write(&ws.join("subprocess.txt"))).await,
        cell(&fixture, TEMP_WRITE).await,
    ];
    assert_eq!(
        results,
        [
            "'errno 13'".to_string(),
            "'errno 13'".to_string(),
            "(1, True)".to_string(),
            "1".to_string(),
            "'tmp ok'".to_string(),
        ]
    );
    let written: Vec<bool> = ["cell.txt", "ctypes.txt", "bash.txt", "subprocess.txt"]
        .iter()
        .map(|name| ws.join(name).exists())
        .collect();
    assert_eq!(written, [false; 4]);
    fixture.provisioner.dispose(None).await;
}

/// The user cache directory (`XDG_CACHE_HOME`, else `~/.cache`; `~/Library/Caches` on macOS)
/// stays writable in plan mode, as the model-facing message promises: tools like `uv` and
/// `pip` fill it during a dry run. The cell resolves it the way those tools do.
#[tokio::test]
async fn plan_mode_keeps_the_user_cache_dir_writable() {
    let Some(fixture) = fixture(SandboxMode::Off, true) else {
        return;
    };
    let written = cell(
        &fixture,
        "import os, sys, tempfile\n\
         _base = os.environ.get('XDG_CACHE_HOME') or os.path.expanduser(\
         '~/Library/Caches' if sys.platform == 'darwin' else '~/.cache')\n\
         os.makedirs(_base, exist_ok=True)\n\
         try:\n    with tempfile.NamedTemporaryFile('w', dir=_base) as f:\n        \
         f.write('x')\n    _out = 'cache ok'\n\
         except OSError as error:\n    _out = f'errno {error.errno}'\n_out",
    )
    .await;
    assert_eq!(written, "'cache ok'");
    fixture.provisioner.dispose(None).await;
}

/// A workspace inside the temp directory: `read-only` grants the temp directory, and Landlock
/// cannot carve the workspace out of it, so the plan-mode kernel gets a private temp directory
/// in the session's artifact dir instead of the shared one. A cell, `subprocess` and `bash()`
/// still fail with EACCES in the workspace, and temp files keep working.
#[tokio::test]
async fn a_workspace_inside_the_temp_dir_stays_read_only_in_plan_mode() {
    let base = tempfile::tempdir().unwrap();
    assert!(base.path().starts_with(std::env::temp_dir()));
    let Some(fixture) = fixture_in(base, SandboxMode::Off, true) else {
        return;
    };
    let ws = &fixture.workspace;
    let results = [
        cell(&fixture, &python_write(&ws.join("cell.txt"))).await,
        cell(&fixture, &subprocess_write(&ws.join("subprocess.txt"))).await,
        cell(&fixture, &bash_write(&ws.join("bash.txt"))).await,
        cell(&fixture, TEMP_WRITE).await,
    ];
    assert_eq!(
        results,
        [
            "'errno 13'".to_string(),
            "1".to_string(),
            "(1, True)".to_string(),
            "'tmp ok'".to_string(),
        ]
    );
    let written: Vec<bool> = ["cell.txt", "subprocess.txt", "bash.txt"]
        .iter()
        .map(|name| ws.join(name).exists())
        .collect();
    assert_eq!(written, [false; 3]);
    fixture.provisioner.dispose(None).await;
}

#[tokio::test]
async fn toggling_keeps_the_namespace_and_leaving_plan_mode_restores_writes() {
    let Some(fixture) = fixture(SandboxMode::Off, false) else {
        return;
    };
    let ws = &fixture.workspace;
    let before = [
        cell(&fixture, "kept = 41\nkept").await,
        cell(&fixture, &python_write(&ws.join("before.txt"))).await,
    ];
    let unconfined_pid = kernel_pid(&fixture);

    fixture.mode.set(true);
    let entering = Instant::now();
    assert_eq!(
        fixture.provisioner.sync_plan_mode().await.unwrap(),
        PlanModeApplied::Restarted
    );
    let entered_ms = entering.elapsed().as_millis();
    let planning = [
        cell(&fixture, "kept + 1").await,
        cell(&fixture, &python_write(&ws.join("planning.txt"))).await,
        cell(&fixture, &ctypes_write(&ws.join("planning-ctypes.txt"))).await,
    ];
    let confined_pid = kernel_pid(&fixture);

    fixture.mode.set(false);
    let leaving = Instant::now();
    assert_eq!(
        fixture.provisioner.sync_plan_mode().await.unwrap(),
        PlanModeApplied::Restarted
    );
    let left_ms = leaving.elapsed().as_millis();
    let after = [
        cell(&fixture, "kept").await,
        cell(&fixture, &python_write(&ws.join("after.txt"))).await,
    ];
    eprintln!("plan-mode toggle latency: enter {entered_ms} ms, leave {left_ms} ms");
    assert_eq!(
        (before, planning, after),
        (
            ["41".to_string(), "'ok'".to_string()],
            [
                "42".to_string(),
                "'errno 13'".to_string(),
                "'errno 13'".to_string()
            ],
            ["41".to_string(), "'ok'".to_string()],
        )
    );
    // Each policy change replaced the kernel process.
    assert_ne!(unconfined_pid, confined_pid);
    assert_ne!(confined_pid, kernel_pid(&fixture));
    fixture.provisioner.dispose(None).await;
}

#[tokio::test]
async fn a_configured_read_only_sandbox_toggles_without_a_restart() {
    let Some(fixture) = fixture(SandboxMode::ReadOnly, false) else {
        return;
    };
    let kept = cell(&fixture, "kept = 7\nkept").await;
    let pid = kernel_pid(&fixture);
    fixture.mode.set(true);
    assert_eq!(
        fixture.provisioner.sync_plan_mode().await.unwrap(),
        PlanModeApplied::InPlace
    );
    fixture.mode.set(false);
    assert_eq!(
        fixture.provisioner.sync_plan_mode().await.unwrap(),
        PlanModeApplied::InPlace
    );
    assert_eq!(
        (kept, kernel_pid(&fixture), cell(&fixture, "kept").await),
        ("7".to_string(), pid, "7".to_string())
    );
    fixture.provisioner.dispose(None).await;
}

#[tokio::test]
async fn a_busy_kernel_refuses_the_toggle_and_keeps_running() {
    let Some(fixture) = fixture(SandboxMode::Off, false) else {
        return;
    };
    let manager = fixture.provisioner.ensure(None, None).await.unwrap();
    let pid = manager.process_id();
    // A running cell: it announces itself through a file, then waits to be interrupted.
    let started = fixture.workspace.join("started");
    let signal = pa_core::kernel::cancellation::AbortSignal::new();
    let running = {
        let manager = manager.clone();
        let signal = signal.clone();
        let code = format!(
            "import time\nopen({:?}, 'w').close()\ntime.sleep(60)",
            started.display().to_string()
        );
        tokio::spawn(async move {
            manager
                .execute(
                    &code,
                    ExecuteOptions {
                        signal: Some(signal),
                        ..Default::default()
                    },
                )
                .await
        })
    };
    while !started.exists() {
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    fixture.mode.set(true);
    let busy_cell = fixture.provisioner.sync_plan_mode().await;
    signal.abort();
    let _ = running.await;
    // A live background `bash()` job refuses it too: the restart would kill the job.
    fixture.mode.set(false);
    let job = cell(&fixture, "_h = bash('sleep 60')\n_h.pid > 0").await;
    fixture.mode.set(true);
    let busy_job = fixture.provisioner.sync_plan_mode().await;
    let _ = cell(&fixture, "_h.kill()").await;
    let messages = [busy_cell, busy_job].map(|result| match result {
        Ok(_) => "switched".to_string(),
        Err(error) => format!("{error:#}"),
    });
    assert_eq!(job, "True");
    assert_eq!(
        messages,
        [
            "the Python kernel is busy (a cell is running); switching plan mode restarts it \
             under a different OS sandbox. Try again once it finishes."
                .to_string(),
            "the Python kernel is busy (background bash() commands are running); switching \
             plan mode restarts it under a different OS sandbox. Try again once they finish."
                .to_string(),
        ]
    );
    assert_eq!(kernel_pid(&fixture), pid);
    fixture.provisioner.dispose(None).await;
}
