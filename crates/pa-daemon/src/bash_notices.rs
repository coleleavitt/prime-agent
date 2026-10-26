//! The daemon worker's kernel-notice seams (bash completions and factory
//! progress): the notice types, the validation, and the handler
//! registration live in pa-core (`session_engine::bash_notices`); the
//! worker keeps its queue-lane sinks here.

use pa_core::kernel::shared::{HostRequestHandlers, host_handler};
use pa_types::sync::MutexExt;
use serde_json::Value;

use crate::agent_engine::AgentSessionEngine;

impl AgentSessionEngine {
    /// Wire the worker's kernel-notice queue seams (called once at
    /// construction, before the first session build reads them).
    ///
    /// # Panics
    ///
    /// Panics when a sink mutex is poisoned (a holder panicked while
    /// holding the notice or consumed-sink lock).
    pub fn set_kernel_notice_sinks(
        &self,
        notice: crate::engine::KernelNoticeSink,
        consumed: crate::engine::BashConsumedSink,
    ) {
        *self.kernel_notice_sink.lock_or_recover() = Some(notice);
        *self.bash_consumed_sink.lock_or_recover() = Some(consumed);
    }

    /// Wire the worker's monitor-event queue seam (called once at
    /// construction, before the first session build reads it).
    pub fn set_monitor_event_sink(&self, sink: crate::engine::MonitorEventSink) {
        *self.monitor_event_sink.lock_or_recover() = Some(sink);
    }

    /// The `monitor.event` kernel host handler (`rlm.monitor`'s batches):
    /// registered only when the worker wired its queue seam.
    pub(crate) fn register_monitor_host_handler(&self, handlers: &mut HostRequestHandlers) {
        let Some(sink) = self.monitor_event_sink.lock_or_recover().clone() else {
            return;
        };
        handlers.register(
            "monitor.event",
            host_handler(move |payload| {
                let sink = sink.clone();
                Box::pin(async move {
                    sink(validate_monitor_event(&payload.data)?);
                    Ok(serde_json::json!({}))
                })
            }),
        );
    }

    /// Wire the worker's prebuilt notice-row seam (called once at
    /// construction, before the first session build reads it).
    pub fn set_notice_row_sink(&self, sink: crate::engine::NoticeRowSink) {
        *self.notice_row_sink.lock_or_recover() = Some(sink);
    }

    /// The `cell.completed` kernel host handler: a cell moved to the
    /// background ended; its row joins a running turn or wakes the session.
    pub(crate) fn register_cell_completed_host_handler(&self, handlers: &mut HostRequestHandlers) {
        let Some(sink) = self.notice_row_sink.lock_or_recover().clone() else {
            return;
        };
        handlers.register(
            "cell.completed",
            host_handler(move |payload| {
                let sink = sink.clone();
                Box::pin(async move {
                    let completion = validate_cell_completed(&payload.data)?;
                    sink(
                        pa_core::session_engine::messages::create_background_cell_message(
                            &completion,
                            crate::util::now_ms(),
                        ),
                    );
                    Ok(serde_json::json!({}))
                })
            }),
        );
    }

    /// Wire the worker's presented-artifact row sink (`artifact.present`):
    /// called once at construction, before the first session build.
    pub fn set_presented_artifact_sink(
        &self,
        sink: pa_core::session_engine::presented_artifact::PresentedArtifactSink,
    ) {
        *self.presented_artifact_sink.lock_or_recover() = Some(sink);
    }

    /// The `bash.completed`/`bash.consumed`/`factory.progress` kernel host
    /// handlers. Registered only when both seams are wired — no worker queue
    /// leaves the requests honestly unavailable.
    pub(crate) fn register_kernel_notice_host_handlers(&self, handlers: &mut HostRequestHandlers) {
        let Some(notice) = self.kernel_notice_sink.lock_or_recover().clone() else {
            return;
        };
        let Some(consumed) = self.bash_consumed_sink.lock_or_recover().clone() else {
            return;
        };
        pa_core::session_engine::bash_notices::register_kernel_notice_host_handlers(
            notice, consumed, handlers,
        );
    }
}

/// The most characters of a finished background cell's result, error or
/// output the host keeps (the kernel bounds them first).
const CELL_COMPLETED_MAX_CHARS: usize = 8_000;

/// `cell.completed` validation: a non-empty cell id and an `ok`/`error`
/// status; the texts are optional and bounded.
fn validate_cell_completed(
    data: &Value,
) -> anyhow::Result<pa_core::session_engine::messages::BackgroundCellCompletion> {
    let cell_id = data
        .get("cellId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("cell.completed cellId must be a non-empty string"))?;
    let status = data
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| matches!(*status, "ok" | "error"))
        .ok_or_else(|| anyhow::anyhow!("cell.completed status must be \"ok\" or \"error\""))?;
    let text = |key: &str| {
        data.get(key).and_then(Value::as_str).map(|text| {
            let skip = text
                .chars()
                .count()
                .saturating_sub(CELL_COMPLETED_MAX_CHARS);
            text.chars().skip(skip).collect::<String>()
        })
    };
    Ok(
        pa_core::session_engine::messages::BackgroundCellCompletion {
            cell_id: cell_id.to_string(),
            status: status.to_string(),
            result: text("result"),
            error: text("error"),
            output: text("output").unwrap_or_default(),
            duration_ms: data.get("durationMs").and_then(Value::as_u64).unwrap_or(0),
        },
    )
}

/// The most characters of one monitor line the host keeps (the kernel cuts
/// at 500; this bounds a runtime that does not).
const MONITOR_LINE_MAX_CHARS: usize = 1_000;

/// `monitor.event` validation: a non-empty monitor id, string lines (cut to
/// [`MONITOR_LINE_MAX_CHARS`]), an optional dropped count and end object.
fn validate_monitor_event(
    data: &Value,
) -> anyhow::Result<pa_core::session_engine::messages::MonitorEvent> {
    let monitor_id = data
        .get("monitorId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("monitor.event monitorId must be a non-empty string"))?;
    let lines = data
        .get("lines")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("monitor.event lines must be an array"))?
        .iter()
        .map(|line| {
            line.as_str()
                .map(|line| line.chars().take(MONITOR_LINE_MAX_CHARS).collect())
                .ok_or_else(|| anyhow::anyhow!("monitor.event lines must be strings"))
        })
        .collect::<anyhow::Result<Vec<String>>>()?;
    let mut event = pa_core::session_engine::messages::MonitorEvent {
        monitor_id: monitor_id.to_string(),
        description: data
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect(),
        lines: Vec::new(),
        dropped: data.get("dropped").and_then(Value::as_u64).unwrap_or(0),
        end: data.get("end").filter(|end| end.is_object()).cloned(),
    };
    // Through the merge: one oversized batch keeps the row's line cap too.
    event.merge(pa_core::session_engine::messages::MonitorEvent {
        lines,
        ..Default::default()
    });
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_event_validation_bounds_the_batch() {
        let event = validate_monitor_event(&serde_json::json!({
            "monitorId": "m1",
            "description": "tests",
            "lines": ["a", "x".repeat(5_000)],
            "dropped": 3,
            "end": {"reason": "exited", "exitCode": 0},
        }))
        .unwrap();
        assert_eq!(event.monitor_id, "m1");
        assert_eq!(event.lines[1].chars().count(), MONITOR_LINE_MAX_CHARS);
        assert_eq!(event.dropped, 3);
        assert_eq!(event.end.unwrap()["reason"], "exited");
        assert!(
            validate_monitor_event(&serde_json::json!({"monitorId": "", "lines": []})).is_err()
        );
        assert!(
            validate_monitor_event(&serde_json::json!({"monitorId": "m1", "lines": [1]})).is_err()
        );
    }

    #[test]
    fn cell_completed_validation_keeps_the_end_of_long_texts() {
        let completion = validate_cell_completed(&serde_json::json!({
            "cellId": "c1",
            "status": "error",
            "error": "Traceback ...\nValueError: bad",
            "output": format!("{}tail", "x".repeat(20_000)),
            "durationMs": 1500,
        }))
        .unwrap();
        assert_eq!(completion.status, "error");
        assert_eq!(completion.output.chars().count(), CELL_COMPLETED_MAX_CHARS);
        assert!(completion.output.ends_with("tail"));
        assert_eq!(completion.duration_ms, 1500);
        assert!(
            validate_cell_completed(&serde_json::json!({"cellId": "c1", "status": "done"}))
                .is_err()
        );
        assert!(validate_cell_completed(&serde_json::json!({"status": "ok"})).is_err());
    }
}
