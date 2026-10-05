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

//! Verifier integration tests for the ipython tool's escalation past an ignored interrupt (#2135):
//! a kernel that ignores the interrupt after a cancel is killed so the next call gets a fresh
//! kernel instead of "still running the previously interrupted cell". The kernel Python is ambient
//! product state; skipped when absent.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pa_core::kernel::provisioner::{IpythonKernelProvisioner, IpythonKernelProvisionerOptions};
use pa_core::{create_ipython_tool_definition, IpythonToolOptions, ToolContentBlock};
use serde_json::{json, Value};

/// The kernel Python with prime-agent-runtime installed; set
/// `PA_CORE_KERNEL_PYTHON` to point at an explicit interpreter instead.
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
    let home = std::env::var("HOME").unwrap_or_default();
    let candidate = PathBuf::from(format!("{home}/.prime/agent/kernel-venv/bin/python"));
    if candidate.exists() {
        return Some(candidate);
    }
    eprintln!(
        "kernel python {} not found; skipping live cell-timeout test",
        candidate.display()
    );
    None
}

/// A cell that swallows every interrupt: only a kill stops it.
const UNRESPONSIVE_CELL: &str = "import time\nwhile True:\n    try:\n        time.sleep(0.05)\n    except BaseException:\n        pass\n";

struct Harness {
    _dir: tempfile::TempDir,
    tool: pa_core::ToolDefinition,
}

impl Harness {
    fn new(python: PathBuf) -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let provisioner = IpythonKernelProvisioner::new(
            dir.path(),
            IpythonKernelProvisionerOptions {
                python: Some(python),
                ..Default::default()
            },
        );
        let tool = create_ipython_tool_definition(
            "/tmp",
            IpythonToolOptions {
                provisioner: Arc::new(provisioner),
                ui: None,
                on_late_sent_agent_message: None,
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
async fn a_cancelled_cell_that_ignores_the_interrupt_is_killed_so_later_calls_are_not_busy() {
    let Some(python) = kernel_python() else {
        return;
    };
    let harness = Harness::new(python);
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
        (details["status"].clone(), details["kernelKilled"].clone()),
        (json!("aborted"), json!(true)),
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
