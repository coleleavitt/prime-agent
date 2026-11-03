// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines/casts).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! Per-cell auto-background integration tests: Claude Code's `fHe`
//! `setTimeout` shape ported to the persistent Python kernel. A cell that
//! yields to the event loop past the budget is moved to the background by
//! writing one `detach` frame; the kernel's `_run_guarded` returns
//! "detached" and the request settles ok with the `[Still running...]`
//! result text. A `time.sleep(N)` cell (synchronous, cannot yield) stays
//! on the active slot — detach is a no-op there, matching the design.
#![cfg(unix)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions,
    ExecuteStatus,
    HostRequestHandlers,
    KernelManagerOptions,
};

/// The kernel Python with prime-agent-runtime installed. Ambient product
/// state, not test input: skipped (with a note) when absent;
/// `PA_CORE_KERNEL_PYTHON` points at an explicit interpreter instead.
fn kernel_python() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("PA_CORE_KERNEL_PYTHON") {
        let explicit = PathBuf::from(explicit);
        assert!(
            explicit.exists(),
            "PA_CORE_KERNEL_PYTHON {} not found",
            explicit.display()
        );
        return Some(explicit);
    }
    let candidate = PathBuf::from(std::env::var("HOME").map_or_else(
        |_| "/home/ubuntu/.prime/agent/kernel-venv/bin/python".to_string(),
        |home| format!("{home}/.prime/agent/kernel-venv/bin/python"),
    ));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live kernel test",
        candidate.display()
    );
    None
}

async fn started_manager(session: &str) -> Option<ReplKernelManager> {
    let python = kernel_python()?;
    let options = KernelManagerOptions {
        python: Some(python),
        cwd: Some(std::env::temp_dir()),
        env: HashMap::new(),
        session_id: Some(session.to_string()),
        host_handlers: HostRequestHandlers::new(),
        python_skills: Vec::new(),
        on_background_work_settled: None,
        snapshot: None,
        bootstrap_code: None,
        stderr_log_path: None,
        ..Default::default()
    };
    let manager = ReplKernelManager::new(options);
    manager
        .start(KernelStartOptions::default())
        .await
        .expect("kernel must start");
    Some(manager)
}

/// An `async` cell that yields to the loop (`asyncio.sleep`) past the
/// auto-background budget must settle ok via the `[detached]` path well
/// before any hard interrupt could land.
#[tokio::test]
async fn async_cell_past_the_budget_is_detached_and_settles_ok() {
    let Some(manager) = started_manager("cell-auto-bg-async").await else {
        return;
    };

    let started = Instant::now();
    let result = manager
        .execute(
            "import asyncio\nawait asyncio.sleep(60)\n'done'",
            ExecuteOptions {
                auto_background_ms: Some(500),
                ..ExecuteOptions::default()
            },
        )
        .await
        .expect("execute must not fail");
    let elapsed = started.elapsed();

    assert_eq!(result.status, ExecuteStatus::Ok);
    assert!(
        result.auto_backgrounded,
        "the auto-background flag must be set on the result: result={result:?}"
    );
    let result_text = result.result.as_deref().unwrap_or("");
    assert!(
        result_text.starts_with("[Still running."),
        "a detached cell carries the `[Still running...]` marker, got {result_text:?}"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the cell must settle via detach well under the 60s asyncio.sleep: elapsed {elapsed:?}"
    );

    manager
        .shutdown(pa_core::kernel::shared::KernelShutdownOptions::default())
        .await
        .expect("shutdown");
}

/// A synchronous `time.sleep(N)` cell holds the event loop thread. The
/// Rust-side auto-background timer still fires and writes a `detach`
/// frame (`auto_backgrounded: true`), but the cell cannot yield to the
/// loop, so the detach is a no-op: `_run_guarded` returns ok with the
/// cell's own result, not the `[Still running...]` marker. This tells
/// the caller "we tried to move it; it did not move," which is the
/// right signal for the hard `cell_timeout_ms` lever to take over.
#[tokio::test]
async fn sync_cell_past_the_budget_fires_the_timer_but_runs_to_completion() {
    let Some(manager) = started_manager("cell-auto-bg-sync").await else {
        return;
    };

    let result = manager
        .execute(
            "import time\ntime.sleep(2)\n'blocked'",
            ExecuteOptions {
                auto_background_ms: Some(300),
                ..ExecuteOptions::default()
            },
        )
        .await
        .expect("execute must not fail");

    assert_eq!(result.status, ExecuteStatus::Ok);
    assert_eq!(
        result.result.as_deref(),
        Some("'blocked'"),
        "the sync cell runs to completion; the detach was a no-op"
    );
    assert!(
        result.auto_backgrounded,
        "the Rust timer fires regardless of whether the cell can yield"
    );
    let result_text = result.result.as_deref().unwrap_or("");
    assert!(
        !result_text.starts_with("[Still running."),
        "a sync-blocked cell is never moved to the background by detach; got {result_text:?}"
    );

    manager
        .shutdown(pa_core::kernel::shared::KernelShutdownOptions::default())
        .await
        .expect("shutdown");
}

/// A sub-budget cell finishes on its own; the auto-background timer must
/// not race ahead and detach an already-settled request.
#[tokio::test]
async fn sub_budget_cell_settles_without_auto_background() {
    let Some(manager) = started_manager("cell-auto-bg-quick").await else {
        return;
    };

    let result = manager
        .execute(
            "import asyncio\nawait asyncio.sleep(0.1)\n'quick'",
            ExecuteOptions {
                auto_background_ms: Some(10_000),
                ..ExecuteOptions::default()
            },
        )
        .await
        .expect("execute must not fail");

    assert_eq!(result.status, ExecuteStatus::Ok);
    assert_eq!(result.result.as_deref(), Some("'quick'"));
    assert!(!result.auto_backgrounded);

    manager
        .shutdown(pa_core::kernel::shared::KernelShutdownOptions::default())
        .await
        .expect("shutdown");
}

/// `None` and `0` disable the timer. An async cell with a generous wall
/// clock runs to its natural end without being moved.
#[tokio::test]
async fn disabled_budget_lets_an_async_cell_run_to_natural_end() {
    let Some(manager) = started_manager("cell-auto-bg-disabled").await else {
        return;
    };

    for budget in [None, Some(0u64)] {
        let result = manager
            .execute(
                "import asyncio\nawait asyncio.sleep(0.2)\n'natural'",
                ExecuteOptions {
                    auto_background_ms: budget,
                    ..ExecuteOptions::default()
                },
            )
            .await
            .expect("execute must not fail");
        assert_eq!(result.status, ExecuteStatus::Ok);
        assert_eq!(result.result.as_deref(), Some("'natural'"));
        assert!(
            !result.auto_backgrounded,
            "budget {budget:?} must not auto-background"
        );
    }

    manager
        .shutdown(pa_core::kernel::shared::KernelShutdownOptions::default())
        .await
        .expect("shutdown");
}

/// The two timers together: a cell the auto-background timer moves first has
/// settled, so the later time limit never reaches it and the background body
/// runs to its end (the limit bounds how long a cell blocks the session).
#[tokio::test]
async fn a_detached_cell_outlives_the_time_limit() {
    let Some(manager) = started_manager("cell-auto-bg-limit-async").await else {
        return;
    };

    let result = manager
        .execute(
            "import asyncio\nfinished = False\nawait asyncio.sleep(2)\nfinished = True",
            ExecuteOptions {
                auto_background_ms: Some(300),
                cell_timeout_ms: Some(1_000),
                ..ExecuteOptions::default()
            },
        )
        .await
        .expect("execute must not fail");
    assert_eq!(result.status, ExecuteStatus::Ok);
    assert!(
        result
            .result
            .as_deref()
            .is_some_and(|text| text.starts_with("[Still running.")),
        "the cell moved before its limit: {result:?}"
    );
    assert_eq!(result.timed_out_after_ms, None);

    // Wait for the background body itself, then read what it left behind.
    let settled = manager
        .execute(
            "import asyncio\nimport rlm.repl as _repl\nawait asyncio.gather(*_repl._detached_bodies.values(), return_exceptions=True)\nfinished",
            ExecuteOptions::default(),
        )
        .await
        .expect("execute must not fail");
    assert_eq!(
        (settled.status, settled.result.as_deref()),
        (ExecuteStatus::Ok, Some("True")),
        "the time limit interrupted a cell that was already in the background: {settled:?}"
    );

    manager
        .shutdown(pa_core::kernel::shared::KernelShutdownOptions::default())
        .await
        .expect("shutdown");
}

/// A synchronous cell cannot move: the auto-background timer fires first, its
/// detach is a no-op, and the time limit then interrupts the cell.
#[tokio::test]
async fn a_cell_that_cannot_move_is_interrupted_at_its_time_limit() {
    let Some(manager) = started_manager("cell-auto-bg-limit-sync").await else {
        return;
    };

    let result = manager
        .execute(
            "import time\ntime.sleep(30)",
            ExecuteOptions {
                auto_background_ms: Some(300),
                cell_timeout_ms: Some(1_000),
                ..ExecuteOptions::default()
            },
        )
        .await
        .expect("execute must not fail");
    assert_eq!(result.status, ExecuteStatus::Error, "{result:?}");
    assert!(result.auto_backgrounded, "the detach was tried first");
    assert_eq!(result.timed_out_after_ms, Some(1_000));

    manager
        .shutdown(pa_core::kernel::shared::KernelShutdownOptions::default())
        .await
        .expect("shutdown");
}
