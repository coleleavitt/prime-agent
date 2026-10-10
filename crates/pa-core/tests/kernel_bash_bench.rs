// Pedantic-gate dispositions as src/lib.rs (large_futures).
#![allow(clippy::large_futures)]
#![cfg(unix)]

//! The kernel `bash()` latency benchmark: a real `python -m rlm.repl` kernel
//! against this host, timing `await bash(...)` inside one cell (so the numbers
//! are what model code sees). Ignored by default; run it in a release build:
//!
//! ```sh
//! cargo test --release -p pa-core --test kernel_bash_bench --locked -- --ignored --nocapture
//! ```
//!
//! `PA_CORE_KERNEL_PYTHON` picks the interpreter (an older runtime's venv, to
//! compare); `PA_BASH_BENCH_RUNS` the run count per case (default 300);
//! `PA_BASH_BENCH_INFLATE_MB` touches that much extra host memory first (spawn
//! cost against host RSS).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use pa_core::kernel::bootstrap::build_rlm_bootstrap_code;
use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions, ExecuteStatus, HostRequestHandlers, KernelManagerOptions, KernelShutdownOptions,
};

fn kernel_python() -> Option<PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

/// Times each case `runs` times (after a warm-up) and prints one line per
/// case: median and p95 in milliseconds.
const BENCH_CELL: &str = r#"
import statistics, time
from rlm.bash import bash
async def _bench(command, runs):
    for _ in range(5):
        try:
            await bash(command)
        except Exception as error:
            warm = f"{type(error).__name__}: {error}"[:80]
        else:
            warm = "ok"
    samples = []
    for _ in range(runs):
        start = time.perf_counter()
        try:
            result = await bash(command)
        except Exception:
            result = None
        samples.append((time.perf_counter() - start) * 1000)
    samples.sort()
    p95 = samples[max(0, int(len(samples) * 0.95) - 1)]
    size = len(result.output) if result is not None else -1
    return f"{command[:40]!r:44} median {statistics.median(samples):7.3f} ms  p95 {p95:7.3f} ms  output {size}  warm-up {warm}"
_lines = []
for _command in ("true", "echo hi", "yes 'a line of build output text' | head -c 10000000", "sudo true"):
    _lines.append(await _bench(_command, RUNS))
print("\n".join(_lines))
"#;

#[tokio::test]
#[ignore = "benchmark: run with --release -- --ignored --nocapture"]
async fn kernel_bash_latency() {
    let Some(python) = kernel_python() else {
        eprintln!("no kernel python; skipping the bash benchmark");
        return;
    };
    let runs = std::env::var("PA_BASH_BENCH_RUNS").unwrap_or_else(|_| "300".to_string());
    // Resident ballast: every page touched, so the host's page tables grow
    // the way a busy host's do.
    let inflate_mb: usize = std::env::var("PA_BASH_BENCH_INFLATE_MB")
        .ok()
        .and_then(|mb| mb.parse().ok())
        .unwrap_or(0);
    let ballast = std::hint::black_box(vec![1u8; inflate_mb * 1024 * 1024]);
    let workspace = tempfile::tempdir().unwrap();
    let manager = ReplKernelManager::new(KernelManagerOptions {
        sandbox: None,
        environment: pa_core::kernel::shared::KernelEnvironment::Inherit,
        plan_guard: None,
        python: Some(python),
        cwd: Some(workspace.path().to_path_buf()),
        env: HashMap::new(),
        session_id: Some("bash-bench".to_string()),
        host_handlers: HostRequestHandlers::new(),
        python_skills: Vec::new(),
        on_background_work_settled: None,
        snapshot: None,
        bootstrap_code: Some(build_rlm_bootstrap_code(&[])),
        stderr_log_path: None,
    });
    manager.start(KernelStartOptions::default()).await.unwrap();
    // `PA_BASH_BENCH_CODE` names a file whose cell runs instead (ad-hoc probes).
    let code = std::env::var_os("PA_BASH_BENCH_CODE").map_or_else(
        || BENCH_CELL.replace("RUNS", &runs),
        |path| std::fs::read_to_string(path).unwrap(),
    );
    let result = manager
        .execute(&code, ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(result.status, ExecuteStatus::Ok, "{result:?}");
    let rss_mib = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|statm| statm.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * 4096 / (1024 * 1024));
    println!("host RSS {rss_mib} MiB\n{}", result.stdout);
    drop(ballast);
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        manager.shutdown(KernelShutdownOptions::default()),
    )
    .await;
}
