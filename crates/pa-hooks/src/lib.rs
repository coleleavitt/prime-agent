//! Claude Code-compatible command hooks: the `hooks` settings key runs
//! user-configured commands on session events (`SessionStart`,
//! `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolBatch`,
//! `Stop`, `SubagentStart`, `SubagentStop`, `PreCompact`, `PostCompact`,
//! `Notification`, `SessionEnd`) with Claude Code's JSON on stdin and its
//! answers on stdout / exit code. The kernel's `bash()` commands are
//! presented as the `Bash` tool and the kernel's MCP calls as
//! `mcp__<server>__<tool>`, so existing Claude Code hook scripts work
//! unmodified. A `PreToolUse` `ask` puts the action to the user; a `Stop`
//! `block` keeps the agent going; an `asyncRewake` hook's exit code 2 wakes
//! an idle session. See `README.md` and `docs/hooks-and-plugins.md`.

pub mod children;
pub mod config;
pub mod http;
pub mod kernel_bash;
pub mod rule;
pub mod run;
mod session;
pub mod worktree;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use pa_agent::types::{AgentMessage, ToolResultContent};
use pa_core::features::{
    AgentStopObservation,
    CompactionObservation,
    FeatureFuture,
    FeatureVerdict,
    HostRequestObservation,
    HostRequestOutcome,
    PromptDecision,
    SessionFeature,
    SessionFeatureContext,
    ToolBatchObservation,
    ToolCallObservation,
    ToolResultObservation,
};
use serde_json::{Value, json};

use crate::children::SettledChild;
use crate::config::{HookEvent, HooksConfig};
use crate::run::HookAnswer;
use crate::session::{SessionHooks, StartContext};

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

fn contexts(answers: &[HookAnswer]) -> Vec<String> {
    answers
        .iter()
        .filter_map(|answer| match answer {
            HookAnswer::Pass { context } | HookAnswer::Allow { context } => context.clone(),
            HookAnswer::Ask(_) | HookAnswer::Block(_) => None,
        })
        .collect()
}

fn blocks(answers: &[HookAnswer]) -> Vec<String> {
    answers
        .iter()
        .filter_map(|answer| match answer {
            HookAnswer::Block(reason) => Some(reason.clone()),
            HookAnswer::Pass { .. } | HookAnswer::Allow { .. } | HookAnswer::Ask(_) => None,
        })
        .collect()
}

fn first_block(answers: &[HookAnswer]) -> Option<String> {
    blocks(answers).into_iter().next()
}

/// The `PreToolUse` answers as one verdict: the first refusal, else the
/// questions asked (joined), else allowed.
fn verdict(answers: &[HookAnswer]) -> FeatureVerdict {
    if let Some(reason) = first_block(answers) {
        return FeatureVerdict::Block(reason);
    }
    let asks: Vec<&str> = answers
        .iter()
        .filter_map(|answer| match answer {
            HookAnswer::Ask(reason) => Some(reason.as_str()),
            HookAnswer::Pass { .. } | HookAnswer::Allow { .. } | HookAnswer::Block(_) => None,
        })
        .collect();
    if asks.is_empty() {
        FeatureVerdict::Allow
    } else {
        FeatureVerdict::Ask(asks.join("; "))
    }
}

/// What `PostToolUse`/`PostToolBatch` answers say to the model: a "block"
/// is feedback (the tools already ran), and context joins it.
fn post_tool_notes(answers: Vec<HookAnswer>) -> Vec<String> {
    answers
        .into_iter()
        .filter_map(|answer| match answer {
            HookAnswer::Block(reason) => Some(reason),
            HookAnswer::Pass { context } | HookAnswer::Allow { context } => context,
            HookAnswer::Ask(_) => None,
        })
        .collect()
}

fn fields<const N: usize>(pairs: [(&str, Value); N]) -> serde_json::Map<String, Value> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

/// The hooks feature, installed by the composition root.
#[derive(Default)]
pub struct HooksFeature {
    sessions: Arc<Mutex<HashMap<String, Arc<SessionHooks>>>>,
    /// `SessionEnd` runs still going, for [`SessionFeature::flush`].
    ending: Mutex<Vec<std::thread::JoinHandle<()>>>,
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
                    let mut config = HooksConfig::load(&context.cwd, &context.agent_dir);
                    // Claude Code skill hooks: the engine recorded the `hooks`
                    // blocks of this session's model-visible skills (trust and
                    // shadowing already applied); register each, bounds-checked.
                    for (skill, hooks) in
                        pa_core::features::session_skill_hooks(&context.session_id)
                    {
                        config.add_skill_hooks(&skill, &hooks);
                    }
                    for problem in &config.problems {
                        tracing::warn!(target: "pa_hooks", "{problem}");
                    }
                    Arc::new(SessionHooks::new(
                        config,
                        context.session_id.clone(),
                        context.cwd.clone(),
                    ))
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
            let extra = fields([("tool_name", json!(tool)), ("tool_input", input)]);
            let answers = hooks.fire(HookEvent::PreToolUse, Some(&tool), extra).await;
            let verdict = verdict(&answers);
            // Claude Code tells `Notification` hooks when it asks for a
            // permission.
            if let FeatureVerdict::Ask(reason) = &verdict {
                hooks.notify(
                    HookEvent::Notification,
                    Some("permission_prompt".to_string()),
                    fields([
                        ("message", json!(reason)),
                        ("title", json!(format!("Permission needed for {tool}"))),
                        ("notification_type", json!("permission_prompt")),
                    ]),
                );
            }
            verdict
        })
    }

    /// Fire `PostToolUse` for a tool that already ran; the notes come back
    /// prefixed with `subject` (they reach the model on the enclosing
    /// cell's result).
    fn post_tool_use(
        hooks: Arc<SessionHooks>,
        tool: String,
        input: Value,
        response: Value,
        subject: String,
    ) -> FeatureFuture<Option<String>> {
        Box::pin(async move {
            let extra = fields([
                ("tool_name", json!(tool)),
                ("tool_input", input),
                ("tool_response", response),
            ]);
            let notes =
                post_tool_notes(hooks.fire(HookEvent::PostToolUse, Some(&tool), extra).await);
            (!notes.is_empty()).then(|| format!("Hook feedback on {subject}: {}", notes.join("\n")))
        })
    }

    /// Fire `SubagentStop` for each child that settled.
    fn subagents_stopped(hooks: &Arc<SessionHooks>, settled: Vec<SettledChild>) {
        for SettledChild {
            child,
            last_message,
        } in settled
        {
            let mut extra = fields([
                ("agent_id", json!(child.id)),
                ("agent_type", json!(child.name)),
                ("stop_hook_active", json!(false)),
            ]);
            if let Some(dir) = child.session_dir {
                extra.insert("agent_session_dir".to_string(), json!(dir));
            }
            if let Some(message) = last_message {
                extra.insert("last_assistant_message".to_string(), json!(message));
            }
            hooks.notify(HookEvent::SubagentStop, Some(child.name), extra);
        }
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

/// A custom row's text content.
fn custom_text(payload: &Value) -> String {
    match payload.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
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
                let extra = fields([("source", json!(source))]);
                contexts(
                    &hooks
                        .fire(HookEvent::SessionStart, Some(source), extra)
                        .await,
                )
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
                let extra = fields([("prompt", json!(prompt))]);
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
            gathered.extend(hooks.take_async_contexts());
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
        let extra = fields([
            ("tool_name", json!(tool)),
            ("tool_input", result.args.clone()),
            (
                "tool_response",
                json!({ "content": result_text(&result.content), "is_error": result.is_error }),
            ),
        ]);
        Box::pin(async move {
            let notes =
                post_tool_notes(hooks.fire(HookEvent::PostToolUse, Some(&tool), extra).await);
            (!notes.is_empty()).then(|| notes.join("\n"))
        })
    }

    /// `PostToolUse` for what the kernel ran: a `bash()` command once its
    /// result arrives (as `Bash`), an MCP call once it answered.
    /// `SubagentStart` when `rlm.run` spawned a child, `SubagentStop` when
    /// a collect or roster reply shows one settled. The running commands
    /// and children are tracked for `Stop`'s `background_tasks`.
    fn after_host_request(
        &self,
        context: &Arc<SessionFeatureContext>,
        request: &HostRequestObservation,
        outcome: HostRequestOutcome<'_>,
    ) -> FeatureFuture<Option<String>> {
        let hooks = self.session(context);
        // A detached completion or a kill ends a job whatever the host
        // answered.
        hooks
            .kernel_jobs
            .forget_ended(&request.request_type, &request.data);
        let Ok(reply) = outcome else {
            return Box::pin(async { None });
        };
        match request.request_type.as_str() {
            "rlm.run" => {
                if let Some(child) = hooks.children.started(reply) {
                    let extra = fields([
                        ("agent_id", json!(child.id)),
                        ("agent_type", json!(child.name)),
                    ]);
                    hooks.notify(HookEvent::SubagentStart, Some(child.name), extra);
                }
                return Box::pin(async { None });
            }
            "rlm.collect" | "rlm.list_subagents" => {
                let settled = hooks
                    .children
                    .settled_in_reply(&request.request_type, reply);
                Self::subagents_stopped(&hooks, settled);
                return Box::pin(async { None });
            }
            _ => {}
        }
        let post_tool_use = hooks.config.has(HookEvent::PostToolUse);
        if request.request_type.starts_with("bash.") {
            // Tracked for `Stop`'s background tasks too.
            if !post_tool_use && !hooks.config.has(HookEvent::Stop) {
                return Box::pin(async { None });
            }
            let finished = hooks
                .kernel_jobs
                .observe(&request.request_type, &request.data, reply);
            let Some(finished) = finished.filter(|_| post_tool_use) else {
                return Box::pin(async { None });
            };
            let subject = format!("`{}`", finished.command);
            return Self::post_tool_use(
                hooks,
                "Bash".to_string(),
                json!({ "command": finished.command }),
                finished.response,
                subject,
            );
        }
        if !post_tool_use {
            return Box::pin(async { None });
        }
        match host_request_as_tool(request) {
            Some((tool, input)) => {
                let subject = format!("`{tool}`");
                Self::post_tool_use(hooks, tool, input, reply.clone(), subject)
            }
            None => Box::pin(async { None }),
        }
    }

    /// `Notification` when a background completion notice reaches the
    /// model (a detached `bash()` command's `[bash-done ...]`, a child's
    /// terminal notice), and `SubagentStop` for the child a terminal notice
    /// reports.
    fn on_message_end(&self, context: &Arc<SessionFeatureContext>, message: &AgentMessage) {
        let AgentMessage::Custom(row) = message else {
            return;
        };
        let custom_type = row
            .payload
            .get("customType")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let title = match custom_type {
            pa_core::session_engine::messages::ASYNC_BASH_COMPLETION_CUSTOM_TYPE => {
                "Background command finished"
            }
            pa_core::session_engine::rlm_notices::RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE
            | pa_core::session_engine::rlm_notices::RLM_CHILD_FAILURE_CUSTOM_TYPE => {
                "Subagent finished"
            }
            _ => return,
        };
        let hooks = self.session(context);
        if custom_type != pa_core::session_engine::messages::ASYNC_BASH_COMPLETION_CUSTOM_TYPE {
            let settled = row
                .payload
                .get("details")
                .and_then(|details| hooks.children.settled_by_notice(details));
            Self::subagents_stopped(&hooks, settled.into_iter().collect());
        }
        hooks.notify(
            HookEvent::Notification,
            Some("background_task_completed".to_string()),
            fields([
                ("message", json!(custom_text(&row.payload))),
                ("title", json!(title)),
                ("notification_type", json!("background_task_completed")),
            ]),
        );
    }

    /// `Stop`: a `block` (or exit code 2) keeps the agent going with the
    /// reason; `stop_hook_active` tells the hook a `Stop` already kept it
    /// going in this run.
    fn before_agent_stop(
        &self,
        context: &Arc<SessionFeatureContext>,
        stop: &AgentStopObservation,
    ) -> FeatureFuture<Option<String>> {
        let hooks = self.session(context);
        if !hooks.config.has(HookEvent::Stop) {
            return Box::pin(async { None });
        }
        let extra = fields([
            ("stop_hook_active", json!(stop.continued)),
            ("last_assistant_message", json!(stop.last_assistant_message)),
            ("background_tasks", hooks.background_tasks()),
        ]);
        Box::pin(async move {
            let reasons = blocks(&hooks.fire(HookEvent::Stop, None, extra).await);
            (!reasons.is_empty()).then(|| format!("Stop hook feedback:\n{}", reasons.join("\n")))
        })
    }

    /// `PostToolBatch`: its context (and any `block` reason, as feedback)
    /// rides the next model request.
    fn after_tool_batch(
        &self,
        context: &Arc<SessionFeatureContext>,
        batch: &ToolBatchObservation,
    ) -> FeatureFuture<Option<String>> {
        let hooks = self.session(context);
        if !hooks.config.has(HookEvent::PostToolBatch) {
            return Box::pin(async { None });
        }
        let calls: Vec<Value> = batch
            .tool_calls
            .iter()
            .map(|call| json!({ "tool_name": call.tool_name, "tool_use_id": call.tool_call_id }))
            .collect();
        let extra = fields([("tool_calls", Value::Array(calls))]);
        Box::pin(async move {
            let notes = post_tool_notes(hooks.fire(HookEvent::PostToolBatch, None, extra).await);
            (!notes.is_empty()).then(|| notes.join("\n"))
        })
    }

    /// `PreCompact`: plain stdout (or `additionalContext`) is added to the
    /// summary instructions.
    fn before_compaction(
        &self,
        context: &Arc<SessionFeatureContext>,
        compaction: &CompactionObservation,
    ) -> FeatureFuture<Option<String>> {
        let hooks = self.session(context);
        if !hooks.config.has(HookEvent::PreCompact) {
            return Box::pin(async { None });
        }
        let trigger = compaction.trigger.as_str();
        let extra = fields([
            ("trigger", json!(trigger)),
            (
                "custom_instructions",
                json!(compaction.custom_instructions.clone().unwrap_or_default()),
            ),
        ]);
        Box::pin(async move {
            let added = contexts(
                &hooks
                    .fire(HookEvent::PreCompact, Some(trigger), extra)
                    .await,
            );
            (!added.is_empty()).then(|| added.join("\n\n"))
        })
    }

    fn after_compaction(
        &self,
        context: &Arc<SessionFeatureContext>,
        compaction: &CompactionObservation,
        summary: &str,
    ) {
        let hooks = self.session(context);
        let trigger = compaction.trigger.as_str();
        hooks.notify(
            HookEvent::PostCompact,
            Some(trigger.to_string()),
            fields([
                ("trigger", json!(trigger)),
                ("compact_summary", json!(summary)),
            ]),
        );
    }

    /// `SessionEnd`, on a thread of its own (the session may end outside
    /// any async runtime); [`Self::flush`] waits for it before the process
    /// exits. The session's hooks stay loaded: a host that rebuilds the
    /// session's engine builds the new one before the old one drops.
    fn on_session_end(&self, context: &Arc<SessionFeatureContext>, reason: &str) {
        let hooks = self.session(context);
        let commands = hooks.config.commands(HookEvent::SessionEnd, Some(reason));
        if commands.is_empty() {
            return;
        }
        let input = hooks.input(HookEvent::SessionEnd, fields([("reason", json!(reason))]));
        match session::run_on_thread(
            HookEvent::SessionEnd,
            commands,
            hooks.cwd.clone(),
            hooks.session_id.clone(),
            input,
        ) {
            Ok(thread) => {
                let mut ending = self.ending.lock().unwrap_or_else(PoisonError::into_inner);
                ending.retain(|thread| !thread.is_finished());
                ending.push(thread);
            }
            Err(error) => {
                tracing::warn!(target: "pa_hooks", "cannot run SessionEnd hooks: {error}");
            }
        }
    }

    fn flush(&self, deadline: Instant) {
        let threads =
            std::mem::take(&mut *self.ending.lock().unwrap_or_else(PoisonError::into_inner));
        for thread in threads {
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(test)]
mod tests;
