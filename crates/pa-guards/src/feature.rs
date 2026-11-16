//! `GuardsFeature`: the `SessionFeature` that wires the guards into a session.
//!
//! - [`SessionFeature::after_tool_call`]: when a kernel cell reports files it
//!   wrote (`fileWrites`), run the scoped slop guard over each and append the
//!   findings to the result the model sees.
//! - [`SessionFeature::after_tool_call`] also appends the steering
//!   reminders of `crate::steer`: structural greps run through subprocess, blocked
//!   fetches, `sed -i` on config or run-time file lists, unbounded bundle
//!   greps, plain-Python writes to source; first match then every fifth.
//! - With `PRIME_AGENT_SOURCE_WRITES=edit-only`,
//!   [`SessionFeature::review_tool_call`] refuses a cell that writes project
//!   source with plain Python before it runs.
//! - [`SessionFeature::before_agent_stop`]: when a turn ends, decide whether the
//!   agent stalled and, if so, return a reason that keeps it going.
//!
//! The task-completion scaffold gate is deliberately left unwired: prime-agent's
//! task state lives in a Python skill, not a native tool, so there is no
//! tool-call seam to gate yet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use pa_core::features::{
    AgentStopObservation,
    FeatureFuture,
    FeatureVerdict,
    SessionFeature,
    SessionFeatureContext,
    ToolCallObservation,
    ToolResultObservation,
};

use crate::steer::{self, Steer};
use crate::{continuation, slop_guard};

/// The source extensions the slop guard reads.
fn is_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("rs" | "ts" | "tsx" | "py" | "go" | "js" | "jsx")
    )
}

/// The previous (committed) content of `path`, for scoping. `None` leaves the
/// slop guard in whole-file mode for that file.
fn git_head_blob(cwd: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(cwd).unwrap_or(path);
    let out = std::process::Command::new("git")
        .args(["show", &format!("HEAD:{}", rel.display())])
        .current_dir(cwd)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The guards feature. Its only state counts the steering matches per
/// session, for the reminders' throttle.
#[derive(Debug, Default)]
pub struct GuardsFeature {
    steer_counts: Mutex<HashMap<(String, Steer), u32>>,
}

impl GuardsFeature {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The steering reminders one python cell's result earns now.
    fn steering(
        &self,
        context: &SessionFeatureContext,
        result: &ToolResultObservation,
    ) -> Vec<String> {
        if result.tool_name != "python_repl" {
            return Vec::new();
        }
        let code = result
            .args
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let strings = |field: &str, key: Option<&str>| -> Vec<String> {
            result
                .host_facts
                .get(field)
                .and_then(serde_json::Value::as_array)
                .map(|records| {
                    records
                        .iter()
                        .filter_map(|record| match key {
                            Some(key) => record.get(key)?.as_str().map(str::to_string),
                            None => record.as_str().map(str::to_string),
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let commands = strings("bashCommands", Some("command"));
        let writes = strings("fileWrites", None);
        let output: String = result
            .content
            .iter()
            .filter_map(|part| match part {
                pa_agent::types::ToolResultContent::Text(text) => Some(text.text.as_str()),
                pa_agent::types::ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let found = steer::detect(&steer::Cell {
            code,
            commands: &commands,
            output: &output,
            writes: &writes,
            project: &context.cwd,
        });
        let mut counts = self
            .steer_counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        found
            .into_iter()
            .filter(|reminder| {
                let count = counts
                    .entry((context.session_id.clone(), reminder.habit))
                    .or_insert(0);
                *count += 1;
                steer::due(*count)
            })
            .map(|reminder| reminder.text)
            .collect()
    }
}

impl SessionFeature for GuardsFeature {
    fn name(&self) -> &'static str {
        "guards"
    }

    fn after_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        result: &ToolResultObservation,
    ) -> FeatureFuture<Option<String>> {
        // Only act on a cell that reported files it wrote.
        let writes: Vec<PathBuf> = result
            .host_facts
            .get("fileWrites")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let reminders = self.steering(context, result);
        if writes.is_empty() {
            let text = (!reminders.is_empty()).then(|| reminders.join("\n"));
            return Box::pin(async move { text });
        }
        let cwd = context.cwd.clone();
        Box::pin(async move {
            let mut out = String::new();
            for path in writes.iter().filter(|p| is_source(p)) {
                let Ok(src_text) = std::fs::read_to_string(path) else {
                    continue;
                };
                let old = git_head_blob(&cwd, path);
                let report =
                    slop_guard::run_all_checks_with_old(path, &src_text, old.as_deref(), &cwd);
                let body = slop_guard::format_report(&report);
                if !body.is_empty() {
                    let name = path.strip_prefix(&cwd).unwrap_or(path).display();
                    out.push_str(&format!("{name}\n{body}"));
                }
            }
            let mut parts: Vec<String> = reminders;
            if !out.is_empty() {
                parts.push(format!("Slop Guard (scoped to your changes):\n{out}"));
            }
            (!parts.is_empty()).then(|| parts.join("\n"))
        })
    }

    fn review_tool_call(
        &self,
        _context: &Arc<SessionFeatureContext>,
        call: &ToolCallObservation,
    ) -> FeatureFuture<FeatureVerdict> {
        let edit_only = std::env::var(steer::SOURCE_WRITES_ENV)
            .is_ok_and(|value| value.trim().eq_ignore_ascii_case("edit-only"));
        let refusal = (call.tool_name == "python_repl")
            .then(|| call.args.get("code").and_then(serde_json::Value::as_str))
            .flatten()
            .and_then(|code| steer::refuse_plain_source_write(code, edit_only));
        Box::pin(async move { refusal.map_or(FeatureVerdict::Allow, FeatureVerdict::Block) })
    }

    fn on_session_end(&self, context: &Arc<SessionFeatureContext>, _reason: &str) {
        self.steer_counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(session_id, _), _| *session_id != context.session_id);
    }

    fn before_agent_stop(
        &self,
        context: &Arc<SessionFeatureContext>,
        stop: &AgentStopObservation,
    ) -> FeatureFuture<Option<String>> {
        let text = stop.last_assistant_message.clone();
        let already = stop.continued;
        let mode = pa_core::settings::SettingsManager::create(&context.cwd, &context.agent_dir)
            .experimental()
            .continue_mode;
        Box::pin(async move {
            if text.trim().is_empty()
                || continuation::looks_like_provider_error(&text)
                || continuation::assistant_is_waiting(&text)
            {
                return None;
            }
            // Do not continue twice in a run unless explicitly allowed.
            if already
                && !matches!(
                    std::env::var("PA_GUARDS_CONTINUE_REPEAT").as_deref(),
                    Ok("1")
                )
            {
                return None;
            }
            mode.text_stalls(&text)
                .then(|| continuation::ContinueReason::Stalled.nudge())
        })
    }
}
