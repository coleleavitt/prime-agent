//! `GuardsFeature`: the `SessionFeature` that wires the guards into a session.
//!
//! - [`SessionFeature::after_tool_call`]: when a kernel cell reports files it
//!   wrote (`fileWrites`), run the scoped slop guard over each and append the
//!   findings to the result the model sees.
//! - [`SessionFeature::before_agent_stop`]: when a turn ends, decide whether the
//!   agent stalled and, if so, return a reason that keeps it going.
//!
//! The task-completion scaffold gate is deliberately left unwired: prime-agent's
//! task state lives in a Python skill, not a native tool, so there is no
//! tool-call seam to gate yet.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_core::features::{
    AgentStopObservation,
    FeatureFuture,
    SessionFeature,
    SessionFeatureContext,
    ToolResultObservation,
};

use crate::{continuation, slop_guard};

/// How aggressively to self-continue, from `PA_GUARDS_CONTINUE_MODE`.
fn continue_mode() -> continuation::ContinueMode {
    match std::env::var("PA_GUARDS_CONTINUE_MODE").as_deref() {
        Ok("autonomous") => continuation::ContinueMode::Autonomous,
        _ => continuation::ContinueMode::Interactive,
    }
}

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

/// The guards feature. Stateless: the pure logic owns no per-session state.
#[derive(Debug, Default)]
pub struct GuardsFeature;

impl GuardsFeature {
    #[must_use]
    pub fn new() -> Self {
        Self
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
        if writes.is_empty() {
            return Box::pin(async { None });
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
            (!out.is_empty()).then(|| format!("Slop Guard (scoped to your changes):\n{out}"))
        })
    }

    fn before_agent_stop(
        &self,
        _context: &Arc<SessionFeatureContext>,
        stop: &AgentStopObservation,
    ) -> FeatureFuture<Option<String>> {
        let text = stop.last_assistant_message.clone();
        let already = stop.continued;
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
            continue_mode()
                .text_stalls(&text)
                .then(|| continuation::ContinueReason::Stalled.nudge())
        })
    }
}
