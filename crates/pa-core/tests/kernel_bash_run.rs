// Pedantic-gate dispositions as src/lib.rs (large_futures).
#![allow(clippy::large_futures)]
#![cfg(unix)]

//! `bash()` in a real kernel against this host: a short command is exactly
//! one host request (`bash.run`: checked, spawned, finished and reaped), and
//! an interrupt while the run waits kills the command (the host's
//! `host_cancel` path) before the cell ends interrupted. Skips without the
//! kernel Python.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pa_core::kernel::bootstrap::build_rlm_bootstrap_code;
use pa_core::kernel::cancellation::AbortSignal;
use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions, ExecuteStatus, HostRequestHandlers, KernelManagerOptions, KernelShutdownOptions,
};

fn kernel_python() -> Option<PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

async fn kernel(workspace: &std::path::Path) -> Option<ReplKernelManager> {
    let manager = ReplKernelManager::new(KernelManagerOptions {
        sandbox: None,
        environment: pa_core::kernel::shared::KernelEnvironment::Inherit,
        plan_guard: None,
        python: Some(kernel_python()?),
        cwd: Some(workspace.to_path_buf()),
        env: HashMap::new(),
        session_id: Some("bash-run-test".to_string()),
        host_handlers: HostRequestHandlers::new(),
        python_skills: Vec::new(),
        on_background_work_settled: None,
        snapshot: None,
        bootstrap_code: Some(build_rlm_bootstrap_code(&[])),
        stderr_log_path: None,
    });
    manager.start(KernelStartOptions::default()).await.unwrap();
    Some(manager)
}

async fn shutdown(manager: &ReplKernelManager) {
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        manager.shutdown(KernelShutdownOptions::default()),
    )
    .await;
}

/// Counts the host requests the cell's `bash()` calls send.
const COUNT_CELL: &str = r#"
import sys
from rlm import repl
from rlm.bash import bash
_types = []
_blocking, _async = repl.host_request_blocking, repl.host_request
def _count_blocking(data, **kwargs):
    _types.append(data.get("type"))
    return _blocking(data, **kwargs)
async def _count_async(data, **kwargs):
    _types.append(data.get("type"))
    return await _async(data, **kwargs)
await bash("true")  # the environment's first trip
repl.host_request_blocking, repl.host_request = _count_blocking, _count_async
try:
    _result = await bash("echo hi")
finally:
    repl.host_request_blocking, repl.host_request = _blocking, _async
print([t for t in _types if t.startswith("bash.")], repr(_result.output))
"#;

#[tokio::test]
async fn a_short_bash_is_one_host_request() {
    let workspace = tempfile::tempdir().unwrap();
    let Some(manager) = kernel(workspace.path()).await else {
        return;
    };
    let result = manager
        .execute(COUNT_CELL, ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(result.status, ExecuteStatus::Ok, "{result:?}");
    assert_eq!(result.stdout.trim(), "['bash.run'] 'hi\\n'");
    shutdown(&manager).await;
}

#[tokio::test]
async fn an_interrupt_during_the_run_kills_the_command() {
    let workspace = tempfile::tempdir().unwrap();
    let Some(manager) = kernel(workspace.path()).await else {
        return;
    };
    let marker = workspace.path().join("marker");
    let pid_file = workspace.path().join("pid");
    // A window far longer than the interrupt's delay: the interrupt lands
    // while the host is still inside the run.
    let cell = format!(
        "import sys\nsys.modules['rlm.bash']._RUN_WINDOW_MS = 20000\nfrom rlm.bash import bash\n\
         await bash(\"echo $$ > '{}'; sleep 3 && touch '{}'\")",
        pid_file.display(),
        marker.display()
    );
    let signal = AbortSignal::new();
    let aborter = {
        let signal = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            signal.abort();
        })
    };
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        manager.execute(
            &cell,
            ExecuteOptions {
                signal: Some(signal),
                ..ExecuteOptions::default()
            },
        ),
    )
    .await
    .expect("the interrupted cell settles")
    .unwrap();
    aborter.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_ne!(result.status, ExecuteStatus::Ok, "{result:?}");
    let pgid: i32 = std::fs::read_to_string(&pid_file)
        .expect("the command started")
        .trim()
        .parse()
        .unwrap();
    let alive = std::process::Command::new("kill")
        .args(["-0", "--", &format!("-{pgid}")])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the command's group outlived the interrupt");
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert!(!marker.exists(), "the killed command still ran to its end");
    // The kernel is still usable.
    let after = manager
        .execute(
            "from rlm.bash import bash\n(await bash('echo ok')).output",
            ExecuteOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(after.status, ExecuteStatus::Ok, "{after:?}");
    shutdown(&manager).await;
}
