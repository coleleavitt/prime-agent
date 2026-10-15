//! Claude Code-compatible command hooks: the `hooks` settings key runs
//! user-configured commands on session events (`SessionStart`,
//! `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`) with Claude
//! Code's JSON on stdin and its answers on stdout / exit code. The kernel's
//! `bash()` commands are presented as the `Bash` tool and the kernel's MCP
//! calls as `mcp__<server>__<tool>`, so existing Claude Code hook scripts
//! work unmodified. See `README.md` and `docs/hooks-and-plugins.md`.

pub mod config;
pub mod run;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use pa_agent::types::{AgentMessage, ToolResultContent};
use pa_core::features::{
    FeatureFuture,
    FeatureVerdict,
    HostRequestObservation,
    PromptDecision,
    SessionFeature,
    SessionFeatureContext,
    ToolCallObservation,
    ToolResultObservation,
};
use serde_json::{Value, json};

use crate::config::{HookEvent, HooksConfig};
use crate::run::HookAnswer;

/// The feature's name (`SessionFeature::name`).
pub const FEATURE_NAME: &str = "hooks";

/// The tool name and input a kernel host request is presented to hooks as;
/// `None` for requests hooks do not see.
#[must_use]
pub fn host_request_as_tool(request: &HostRequestObservation) -> Option<(String, Value)> {
    let field = |key: &str| request.data.get(key).and_then(Value::as_str);
    match request.request_type.as_str() {
        "bash.run" => Some((
            "Bash".to_string(),
            json!({ "command": field("command").unwrap_or_default() }),
        )),
        "mcp.session.call_tool" => Some((
            format!(
                "mcp__{}__{}",
                field("server").unwrap_or_default(),
                field("tool").unwrap_or_default()
            ),
            request
                .data
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({})),
        )),
        _ => None,
    }
}

/// One session's hooks.
struct SessionHooks {
    config: HooksConfig,
    session_id: String,
    cwd: PathBuf,
    /// The `SessionStart` run's context, carried by the first admitted
    /// prompt.
    start: tokio::sync::Mutex<StartContext>,
}

/// Where the session's `SessionStart` context is.
enum StartContext {
    /// No `SessionStart` hooks, or the context was delivered.
    None,
    Running(tokio::task::JoinHandle<Vec<String>>),
    /// Finished, waiting for a prompt to be admitted.
    Ready(Vec<String>),
}

impl SessionHooks {
    fn base_input(&self, event: HookEvent) -> serde_json::Map<String, Value> {
        let mut input = serde_json::Map::new();
        input.insert("session_id".to_string(), json!(self.session_id));
        // The session's conversation log is the daemon's; hooks get none.
        input.insert("transcript_path".to_string(), json!(""));
        input.insert("cwd".to_string(), json!(self.cwd));
        input.insert("hook_event_name".to_string(), json!(event.name()));
        input.insert("permission_mode".to_string(), json!("default"));
        input
    }

    /// Run every command `event` selects for `tool`, in parallel; the
    /// answers come back in configuration order.
    async fn fire(
        &self,
        event: HookEvent,
        tool: Option<&str>,
        extra: serde_json::Map<String, Value>,
    ) -> Vec<HookAnswer> {
        let commands = self.config.commands(event, tool);
        if commands.is_empty() {
            return Vec::new();
        }
        let mut input = self.base_input(event);
        input.extend(extra);
        let input = Value::Object(input);
        let runs = commands.iter().map(|hook| {
            let input = &input;
            async move {
                let outcome = run::run(hook, &self.cwd, &self.session_id, input).await;
                run::answer(event, hook, &outcome)
            }
        });
        futures::future::join_all(runs).await
    }
}

fn contexts(answers: &[HookAnswer]) -> Vec<String> {
    answers
        .iter()
        .filter_map(|answer| match answer {
            HookAnswer::Pass { context } | HookAnswer::Allow { context } => context.clone(),
            HookAnswer::Block(_) => None,
        })
        .collect()
}

fn first_block(answers: &[HookAnswer]) -> Option<String> {
    answers.iter().find_map(|answer| match answer {
        HookAnswer::Block(reason) => Some(reason.clone()),
        HookAnswer::Pass { .. } | HookAnswer::Allow { .. } => None,
    })
}

/// The hooks feature, installed by the composition root.
#[derive(Default)]
pub struct HooksFeature {
    sessions: Arc<Mutex<HashMap<String, Arc<SessionHooks>>>>,
}

impl HooksFeature {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The session's hooks, loaded from settings on first use.
    fn session(&self, context: &SessionFeatureContext) -> Arc<SessionHooks> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            sessions
                .entry(context.session_id.clone())
                .or_insert_with(|| {
                    let config = HooksConfig::load(&context.cwd, &context.agent_dir);
                    for problem in &config.problems {
                        tracing::warn!(target: "pa_hooks", "{problem}");
                    }
                    Arc::new(SessionHooks {
                        config,
                        session_id: context.session_id.clone(),
                        cwd: context.cwd.clone(),
                        start: tokio::sync::Mutex::new(StartContext::None),
                    })
                }),
        )
    }

    fn pre_tool_use(
        &self,
        context: &SessionFeatureContext,
        tool: String,
        input: Value,
    ) -> FeatureFuture<FeatureVerdict> {
        let hooks = self.session(context);
        if !hooks.config.has(HookEvent::PreToolUse) {
            return Box::pin(async { FeatureVerdict::Allow });
        }
        Box::pin(async move {
            let mut extra = serde_json::Map::new();
            extra.insert("tool_name".to_string(), json!(tool));
            extra.insert("tool_input".to_string(), input);
            let answers = hooks.fire(HookEvent::PreToolUse, Some(&tool), extra).await;
            first_block(&answers).map_or(FeatureVerdict::Allow, FeatureVerdict::Block)
        })
    }
}

fn result_text(content: &[ToolResultContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl SessionFeature for HooksFeature {
    fn name(&self) -> &'static str {
        FEATURE_NAME
    }

    fn on_session_start(&self, context: &Arc<SessionFeatureContext>, history: &[AgentMessage]) {
        let hooks = self.session(context);
        if !hooks.config.has(HookEvent::SessionStart) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let source = if history.is_empty() {
            "startup"
        } else {
            "resume"
        };
        let task = {
            let hooks = Arc::clone(&hooks);
            runtime.spawn(async move {
                let mut extra = serde_json::Map::new();
                extra.insert("source".to_string(), json!(source));
                contexts(&hooks.fire(HookEvent::SessionStart, None, extra).await)
            })
        };
        // A statement, not the tail: the guard must drop before `hooks`.
        if let Ok(mut start) = hooks.start.try_lock() {
            *start = StartContext::Running(task);
        };
    }

    fn prompt_context(
        &self,
        context: &Arc<SessionFeatureContext>,
        prompt: &str,
    ) -> FeatureFuture<PromptDecision> {
        let hooks = self.session(context);
        let prompt = prompt.to_string();
        Box::pin(async move {
            // The session's start context rides the first admitted prompt:
            // the lock is held across the submit hooks so two prompts cannot
            // both claim it.
            let mut start = hooks.start.lock().await;
            *start = match std::mem::replace(&mut *start, StartContext::None) {
                StartContext::Running(task) => StartContext::Ready(task.await.unwrap_or_default()),
                settled @ (StartContext::Ready(_) | StartContext::None) => settled,
            };
            let mut submitted = Vec::new();
            if hooks.config.has(HookEvent::UserPromptSubmit) {
                let mut extra = serde_json::Map::new();
                extra.insert("prompt".to_string(), json!(prompt));
                let answers = hooks.fire(HookEvent::UserPromptSubmit, None, extra).await;
                if let Some(reason) = first_block(&answers) {
                    return PromptDecision::Block(reason);
                }
                submitted = contexts(&answers);
            }
            let mut gathered = match std::mem::replace(&mut *start, StartContext::None) {
                StartContext::Ready(context) => context,
                StartContext::None | StartContext::Running(_) => Vec::new(),
            };
            drop(start);
            gathered.extend(submitted);
            PromptDecision::Continue {
                context: (!gathered.is_empty()).then(|| gathered.join("\n\n")),
            }
        })
    }

    fn review_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        call: &ToolCallObservation,
    ) -> FeatureFuture<FeatureVerdict> {
        self.pre_tool_use(context, call.tool_name.clone(), call.args.clone())
    }

    fn review_host_request(
        &self,
        context: &Arc<SessionFeatureContext>,
        request: &HostRequestObservation,
    ) -> FeatureFuture<FeatureVerdict> {
        match host_request_as_tool(request) {
            Some((tool, input)) => self.pre_tool_use(context, tool, input),
            None => Box::pin(async { FeatureVerdict::Allow }),
        }
    }

    fn after_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        result: &ToolResultObservation,
    ) -> FeatureFuture<Option<String>> {
        let hooks = self.session(context);
        if !hooks.config.has(HookEvent::PostToolUse) {
            return Box::pin(async { None });
        }
        let tool = result.tool_name.clone();
        let mut extra = serde_json::Map::new();
        extra.insert("tool_name".to_string(), json!(tool));
        extra.insert("tool_input".to_string(), result.args.clone());
        extra.insert(
            "tool_response".to_string(),
            json!({ "content": result_text(&result.content), "is_error": result.is_error }),
        );
        Box::pin(async move {
            let answers = hooks.fire(HookEvent::PostToolUse, Some(&tool), extra).await;
            // A PostToolUse "block" is feedback for the model (the tool
            // already ran); context joins it.
            let notes: Vec<String> = answers
                .into_iter()
                .filter_map(|answer| match answer {
                    HookAnswer::Block(reason) => Some(reason),
                    HookAnswer::Pass { context } | HookAnswer::Allow { context } => context,
                })
                .collect();
            (!notes.is_empty()).then(|| notes.join("\n"))
        })
    }

    fn on_agent_end(&self, context: &Arc<SessionFeatureContext>) {
        let hooks = self.session(context);
        if !hooks.config.has(HookEvent::Stop) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let mut extra = serde_json::Map::new();
            extra.insert("stop_hook_active".to_string(), json!(false));
            hooks.fire(HookEvent::Stop, None, extra).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_requests_present_as_claude_code_tools() {
        let request = |request_type: &str, data: Value| HostRequestObservation {
            request_type: request_type.to_string(),
            data,
        };
        assert_eq!(
            host_request_as_tool(&request(
                "bash.run",
                json!({ "type": "bash.run", "command": "ls -la", "env": { "SECRET": "x" } })
            )),
            Some(("Bash".to_string(), json!({ "command": "ls -la" })))
        );
        assert_eq!(
            host_request_as_tool(&request(
                "mcp.session.call_tool",
                json!({ "server": "github", "tool": "search", "arguments": { "q": "x" } })
            )),
            Some(("mcp__github__search".to_string(), json!({ "q": "x" })))
        );
        assert_eq!(
            host_request_as_tool(&request("bash.follow", json!({ "id": "j1" }))),
            None
        );
    }
}
