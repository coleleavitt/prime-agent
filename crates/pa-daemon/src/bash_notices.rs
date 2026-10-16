//! The detached kernel bash completion notice (the TS async-bash-completion
//! wake: `bash.completed`/`bash.consumed`): on completion the session injects
//! the `[bash-done pid:N exit:M]` custom row with `queueIfBusy` +
//! `resumeIfIdle`; a later kernel read that reaches the model first sends
//! `bash.consumed`, and the undelivered notice withdraws.

use pa_core::kernel::shared::{HostRequestHandlers, host_handler};
use pa_types::sync::MutexExt;
use serde_json::Value;

use crate::agent_engine::AgentSessionEngine;
use crate::engine::{BashCompletionNotice, BashConsumedNotice};

impl AgentSessionEngine {
    /// Wire the worker's bash-completion queue seams (called once at
    /// construction, before the first session build reads them).
    pub fn set_bash_notice_sinks(
        &self,
        completion: crate::engine::BashCompletionSink,
        consumed: crate::engine::BashConsumedSink,
    ) {
        *self.bash_completion_sink.lock_or_recover() = Some(completion);
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

    /// The `bash.completed`/`bash.consumed` kernel host handlers (TS
    /// `createAsyncBashCompletionHostHandler`/`createAsyncBashConsumedHostHandler`).
    /// Registered only when both seams are wired — no worker queue leaves the requests
    /// honestly unavailable.
    pub(crate) fn register_bash_notice_host_handlers(&self, handlers: &mut HostRequestHandlers) {
        let Some(completion) = self.bash_completion_sink.lock_or_recover().clone() else {
            return;
        };
        let Some(consumed) = self.bash_consumed_sink.lock_or_recover().clone() else {
            return;
        };
        handlers.register(
            "bash.completed",
            host_handler(move |payload| {
                let completion = completion.clone();
                Box::pin(async move {
                    let notice = validate_completion(&payload.data)?;
                    // The closed-session gate lives in the sink: the worker's kill/shutdown
                    // set the marker, and the sink refuses the injection like TS `_disposed`.
                    completion(notice);
                    Ok(serde_json::json!({}))
                })
            }),
        );
        handlers.register(
            "bash.consumed",
            host_handler(move |payload| {
                let consumed = consumed.clone();
                Box::pin(async move {
                    let notice = validate_consumed(&payload.data)?;
                    consumed(notice);
                    Ok(serde_json::json!({}))
                })
            }),
        );
    }
}

/// TS `createAsyncBashCompletionHostHandler` validation: a positive
/// integer pid, a non-empty string command, an integer exit code.
fn validate_completion(data: &Value) -> anyhow::Result<BashCompletionNotice> {
    let consumed = validate_consumed(data)?;
    let exit_code = data
        .get("exitCode")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("bash.completed exitCode must be an integer"))?;
    // The output tail is optional: a runtime without it still notifies.
    let output = data
        .get("outputBytes")
        .and_then(Value::as_u64)
        .map(
            |bytes| pa_core::session_engine::messages::BashCompletionOutput {
                tail: data
                    .get("outputTail")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                bytes,
            },
        );
    Ok(BashCompletionNotice {
        pid: consumed.pid,
        command: consumed.command,
        exit_code,
        output,
    })
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

/// TS `createAsyncBashConsumedHostHandler` validation: a positive integer pid and a
/// non-empty command (pids are reused across handles, so the command disambiguates).
fn validate_consumed(data: &Value) -> anyhow::Result<BashConsumedNotice> {
    let pid = data
        .get("pid")
        .and_then(Value::as_u64)
        .filter(|pid| *pid > 0 && u32::try_from(*pid).is_ok())
        .ok_or_else(|| anyhow::anyhow!("bash.completed pid must be a positive integer"))?
        as u32;
    let command = data
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty())
        .ok_or_else(|| anyhow::anyhow!("bash.completed command must be a non-empty string"))?
        .to_string();
    Ok(BashConsumedNotice { pid, command })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_validation_matches_the_ts_contract() {
        assert!(
            validate_completion(&serde_json::json!({
                "pid": 4321,
                "command": "sleep 1",
                "exitCode": 0,
            }))
            .is_ok()
        );
        assert!(
            validate_completion(&serde_json::json!({
                "pid": 0,
                "command": "sleep 1",
                "exitCode": 0,
            }))
            .is_err()
        );
        assert!(
            validate_completion(&serde_json::json!({
                "pid": 4321,
                "command": "",
                "exitCode": 0,
            }))
            .is_err()
        );
        assert!(
            validate_completion(&serde_json::json!({
                "pid": 4321,
                "command": "sleep 1",
                "exitCode": "0",
            }))
            .is_err()
        );
    }

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
