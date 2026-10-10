// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines/casts).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// Drives a real kernel process; unix-only like the sibling kernel targets.
#![cfg(unix)]

//! Verifier integration tests for the ipython tool's per-cell execution timeout (#2290) and the
//! escalation past an ignored interrupt (#2135): a hung cell is interrupted at the timeout; a
//! kernel that ignores the interrupt (after a timeout or a cancel) is killed so the next call gets
//! a fresh kernel instead of "still running the previously interrupted cell"; time the cell spends
//! waiting on a host request (a sub-agent run) does not count. The kernel Python is ambient product
//! state; skipped when absent.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::kernel::shared::{host_handler, HostRequestHandlers};
use pa_core::{create_ipython_tool_definition, IpythonToolOptions, ToolContentBlock};
use serde_json::{json, Value};

/// The kernel Python with prime-agent-runtime installed; set
/// `PA_CORE_KERNEL_PYTHON` to point at an explicit interpreter instead.
fn kernel_python() -> Option<PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

/// A cell that swallows every interrupt: only a kill stops it.
const UNRESPONSIVE_CELL: &str = "import time\nwhile True:\n    try:\n        time.sleep(0.05)\n    except BaseException:\n        pass\n";

struct Harness {
    _dir: tempfile::TempDir,
    tool: pa_core::ToolDefinition,
}

impl Harness {
    fn new(python: PathBuf, cell_timeout_ms: Option<u64>) -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let mut host_handlers = HostRequestHandlers::new();
        // Stands in for a sub-agent run: answers well after the cell timeout.
        host_handlers.register(
            "probe.slow",
            host_handler(|_| async {
                tokio::time::sleep(Duration::from_millis(3_000)).await;
                Ok(json!({ "answered": true }))
            }),
        );
        let provisioner = IpythonKernelProvisioner::new(
            dir.path(),
            IpythonKernelProvisionerOptions {
                python: Some(python),
                host_handlers,
                ..Default::default()
            },
        );
        let tool = create_ipython_tool_definition(
            "/tmp",
            IpythonToolOptions {
                provisioner: Arc::new(provisioner),
                ui: None,
                on_late_sent_agent_message: None,
                cell_timeout_ms,
            },
        );
        Self { _dir: dir, tool }
    }

    /// Run one cell; returns (text, details, `is_error`). Bounded so a regression (a cell that
    /// never times out, a wedged kernel) fails instead of hanging the suite.
    async fn run(
        &self,
        code: &str,
        signal: Option<tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<(String, Value, bool)> {
        let result = tokio::time::timeout(
            Duration::from_secs(60),
            (self.tool.execute)("call", json!({ "code": code }), signal, None),
        )
        .await
        .unwrap_or_else(|_| panic!("cell {code:?} never settled"))?;
        let text = result
            .content
            .iter()
            .filter_map(|block| match block {
                ToolContentBlock::Text { text } => Some(text.as_str()),
                ToolContentBlock::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let mut details = result.details.unwrap_or(Value::Null);
        if let Some(object) = details.as_object_mut() {
            object.remove("durationMs");
        }
        Ok((text, details, result.is_error))
    }
}

#[tokio::test]
async fn a_cell_that_ignores_the_timeout_interrupt_is_killed_and_the_next_call_gets_a_fresh_kernel()
{
    let Some(python) = kernel_python() else {
        return;
    };
    let harness = Harness::new(python, Some(1_500));
    let (_, details, _) = harness.run("x = 41", None).await.unwrap();
    assert_eq!(details["status"], "ok");

    let (text, details, is_error) = harness.run(UNRESPONSIVE_CELL, None).await.unwrap();
    assert_eq!(
        (
            details["status"].clone(),
            details["timedOut"].clone(),
            details["kernelKilled"].clone(),
            is_error
        ),
        (json!("aborted"), json!(true), json!(true), true),
        "{text}"
    );
    assert!(
        text.contains("exceeded the 2s execution timeout")
            && text.contains("did not stop after the interrupt, so it was killed"),
        "{text}"
    );

    // The next call is served by a fresh kernel, not "still running the
    // previously interrupted cell".
    let (text, details, is_error) = harness.run("1 + 1", None).await.unwrap();
    assert_eq!(
        (details["status"].clone(), is_error, text.as_str()),
        (json!("ok"), false, "2"),
    );
}

#[tokio::test]
async fn a_cell_that_honors_the_timeout_interrupt_keeps_the_kernel_state() {
    let Some(python) = kernel_python() else {
        return;
    };
    let harness = Harness::new(python, Some(1_500));
    harness.run("x = 41", None).await.unwrap();
    let (text, details, _) = harness
        .run("import time\ntime.sleep(120)", None)
        .await
        .unwrap();
    assert_eq!(
        (
            details["status"].clone(),
            details["timedOut"].clone(),
            details.get("kernelKilled").cloned()
        ),
        (json!("aborted"), json!(true), None),
        "{text}"
    );
    assert!(text.contains("its state is preserved"), "{text}");
    let (text, _, _) = harness.run("x + 1", None).await.unwrap();
    assert_eq!(text, "42");
}

#[tokio::test]
async fn time_waiting_on_a_host_request_does_not_count_against_the_timeout() {
    let Some(python) = kernel_python() else {
        return;
    };
    let harness = Harness::new(python, Some(1_500));
    let (text, details, is_error) = harness
        .run(
            "from rlm import host_request\n(await host_request('probe.slow'))['answered']",
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        (
            details["status"].clone(),
            details.get("timedOut").cloned(),
            is_error,
            text.as_str()
        ),
        (json!("ok"), None, false, "True"),
    );
}

#[tokio::test]
async fn a_cancelled_cell_that_ignores_the_interrupt_is_killed_so_later_calls_are_not_busy() {
    let Some(python) = kernel_python() else {
        return;
    };
    // No timeout: only the caller's cancel interrupts the cell.
    let harness = Harness::new(python, None);
    harness.run("x = 41", None).await.unwrap();
    let signal = tokio_util::sync::CancellationToken::new();
    let canceller = {
        let signal = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1_000)).await;
            signal.cancel();
        })
    };
    let (text, details, _) = harness.run(UNRESPONSIVE_CELL, Some(signal)).await.unwrap();
    canceller.await.unwrap();
    assert_eq!(
        (
            details["status"].clone(),
            details.get("timedOut").cloned(),
            details["kernelKilled"].clone()
        ),
        (json!("aborted"), None, json!(true)),
        "{text}"
    );
    assert!(
        text.contains(
            "The cancelled cell did not stop after the interrupt, so the Python kernel was killed."
        ),
        "{text}"
    );
    let (text, details, is_error) = harness.run("1 + 1", None).await.unwrap();
    assert_eq!(
        (details["status"].clone(), is_error, text.as_str()),
        (json!("ok"), false, "2"),
    );
}
