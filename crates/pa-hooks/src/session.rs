//! One session's hooks: its configuration, the state the events read
//! (running kernel commands, child agents, the start context), and firing
//! an event's commands, blocking or in the background.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use pa_core::features::{FeatureNotice, deliver_feature_notice};
use serde_json::{Value, json};

use crate::FEATURE_NAME;
use crate::children::Children;
use crate::config::{HookCommand, HookEvent, HookMode, HooksConfig};
use crate::kernel_bash::KernelJobs;
use crate::run::{self, HookAnswer};

/// The most background-hook contexts held for the next prompt; the oldest
/// is dropped first.
const MAX_ASYNC_CONTEXTS: usize = 32;

/// Where the session's `SessionStart` context is.
pub(crate) enum StartContext {
    /// No `SessionStart` hooks, or the context was delivered.
    None,
    Running(tokio::task::JoinHandle<Vec<String>>),
    /// Finished, waiting for a prompt to be admitted.
    Ready(Vec<String>),
}

/// One session's hooks.
pub(crate) struct SessionHooks {
    pub(crate) config: HooksConfig,
    pub(crate) session_id: String,
    pub(crate) cwd: PathBuf,
    /// The `SessionStart` run's context, carried by the first admitted
    /// prompt.
    pub(crate) start: tokio::sync::Mutex<StartContext>,
    /// The kernel's running `bash()` commands, for `PostToolUse` and
    /// `Stop`'s `background_tasks`.
    pub(crate) kernel_jobs: KernelJobs,
    /// The session's RLM child agents.
    pub(crate) children: Children,
    /// Context background (`async`) hooks returned, carried by the next
    /// admitted prompt.
    async_contexts: Mutex<Vec<String>>,
}

/// The text a background hook's exit code 2 wakes the agent with.
fn rewake_text(event: HookEvent, stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("Async {} hook exited with code 2.", event.name())
    } else {
        format!("Async {} hook reported:\n\n{stderr}", event.name())
    }
}

impl SessionHooks {
    pub(crate) fn new(config: HooksConfig, session_id: String, cwd: PathBuf) -> Self {
        Self {
            config,
            session_id,
            cwd,
            start: tokio::sync::Mutex::new(StartContext::None),
            kernel_jobs: KernelJobs::default(),
            children: Children::default(),
            async_contexts: Mutex::default(),
        }
    }

    /// Claude Code's base input plus the event's own fields.
    pub(crate) fn input(&self, event: HookEvent, extra: serde_json::Map<String, Value>) -> Value {
        let mut input = serde_json::Map::new();
        input.insert("session_id".to_string(), json!(self.session_id));
        // The session's conversation log is the daemon's; hooks get none.
        input.insert("transcript_path".to_string(), json!(""));
        input.insert("cwd".to_string(), json!(self.cwd));
        input.insert("hook_event_name".to_string(), json!(event.name()));
        input.insert("permission_mode".to_string(), json!("default"));
        input.extend(extra);
        Value::Object(input)
    }

    /// Run every command `event` selects for `subject`: the blocking ones in
    /// parallel, their answers in configuration order; the background ones
    /// are started and not waited for.
    pub(crate) async fn fire(
        self: &Arc<Self>,
        event: HookEvent,
        subject: Option<&str>,
        extra: serde_json::Map<String, Value>,
    ) -> Vec<HookAnswer> {
        let commands = self.config.commands(event, subject);
        if commands.is_empty() {
            return Vec::new();
        }
        let input = self.input(event, extra);
        let (blocking, background): (Vec<_>, Vec<_>) = commands
            .into_iter()
            .partition(|hook| hook.mode == HookMode::Blocking);
        for hook in background {
            self.start_background(event, hook, input.clone());
        }
        let runs = blocking.iter().map(|hook| {
            let input = &input;
            async move {
                let outcome = run::run(hook, &self.cwd, &self.session_id, input).await;
                run::answer(event, hook, &outcome)
            }
        });
        futures::future::join_all(runs).await
    }

    /// Fire `event` without waiting for it or reading its answers (events
    /// whose output Claude Code ignores, or that have nowhere to deliver
    /// it). Needs an async runtime; without one nothing runs.
    pub(crate) fn notify(
        self: &Arc<Self>,
        event: HookEvent,
        subject: Option<String>,
        extra: serde_json::Map<String, Value>,
    ) {
        if !self.config.has(event) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let hooks = Arc::clone(self);
        runtime.spawn(async move {
            hooks.fire(event, subject.as_deref(), extra).await;
        });
    }

    /// Start one background (`async`) command: its context waits for the
    /// next prompt; an `asyncRewake` command's exit code 2 delivers its
    /// stderr to the model as a notice that wakes an idle session.
    fn start_background(self: &Arc<Self>, event: HookEvent, hook: HookCommand, input: Value) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                target: "pa_hooks",
                event = event.name(),
                command = hook.command.as_str(),
                "an async hook needs an async runtime; skipped"
            );
            return;
        };
        let hooks = Arc::clone(self);
        runtime.spawn(async move {
            let outcome = run::run(&hook, &hooks.cwd, &hooks.session_id, &input).await;
            if hook.mode == HookMode::AsyncRewake && outcome.code == Some(2) {
                let notice = FeatureNotice {
                    feature: FEATURE_NAME.to_string(),
                    text: rewake_text(event, &outcome.stderr),
                };
                if !deliver_feature_notice(&hooks.session_id, &notice) {
                    tracing::warn!(
                        target: "pa_hooks",
                        event = event.name(),
                        command = hook.command.as_str(),
                        "an asyncRewake hook's notice had no live session to wake"
                    );
                }
                return;
            }
            match run::answer(event, &hook, &outcome) {
                HookAnswer::Pass {
                    context: Some(context),
                }
                | HookAnswer::Allow {
                    context: Some(context),
                } => hooks.push_async_context(context),
                HookAnswer::Pass { context: None }
                | HookAnswer::Allow { context: None }
                | HookAnswer::Ask(_)
                | HookAnswer::Block(_) => {}
            }
        });
    }

    fn push_async_context(&self, context: String) {
        let mut contexts = self
            .async_contexts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if contexts.len() >= MAX_ASYNC_CONTEXTS {
            contexts.remove(0);
        }
        contexts.push(context);
    }

    /// The background hooks' contexts, oldest first; none remain.
    pub(crate) fn take_async_contexts(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .async_contexts
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// `Stop`'s `background_tasks`: the kernel's running commands and the
    /// running child agents.
    pub(crate) fn background_tasks(&self) -> Value {
        let jobs = self.kernel_jobs.running().into_iter().map(|job| {
            json!({
                "id": job.id,
                "type": "bash",
                "status": "running",
                "description": job.command,
                "command": job.command,
            })
        });
        let children = self.children.running().into_iter().map(|child| {
            json!({
                "id": child.id,
                "type": "subagent",
                "status": "running",
                "description": child.name,
            })
        });
        Value::Array(jobs.chain(children).collect())
    }
}

/// Run `event`'s commands on a thread of their own (blocking and
/// background alike, every one waited for): for `SessionEnd`, which may
/// fire outside any async runtime, while the process winds down.
pub(crate) fn run_on_thread(
    event: HookEvent,
    commands: Vec<HookCommand>,
    cwd: PathBuf,
    session_id: String,
    input: Value,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("pa-hooks-session-end".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::warn!(target: "pa_hooks", "cannot run {} hooks: {error}", event.name());
                    return;
                }
            };
            runtime.block_on(async {
                let runs = commands.iter().map(|hook| {
                    let (cwd, session_id, input) = (&cwd, &session_id, &input);
                    async move {
                        let outcome = run::run(hook, cwd, session_id, input).await;
                        // Read for its diagnostics: a failure is logged.
                        let _ = run::answer(event, hook, &outcome);
                    }
                });
                futures::future::join_all(runs).await;
            });
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rewake_notice_carries_the_event_and_stderr() {
        assert_eq!(
            rewake_text(HookEvent::PostToolUse, "tests failed\n"),
            "Async PostToolUse hook reported:\n\ntests failed"
        );
        assert_eq!(
            rewake_text(HookEvent::Stop, "  "),
            "Async Stop hook exited with code 2."
        );
    }

    #[test]
    fn background_tasks_list_running_commands_and_children() {
        let hooks = SessionHooks::new(HooksConfig::default(), "s".to_string(), PathBuf::from("/w"));
        hooks.kernel_jobs.observe(
            "bash.run",
            &json!({ "command": "npm run dev" }),
            &json!({ "status": "ok", "job": { "id": "j1", "pid": 7 }, "events": [] }),
        );
        hooks
            .children
            .started(&json!({ "rlm_child_id": "sub-1", "name": "reviewer" }));
        assert_eq!(
            hooks.background_tasks(),
            json!([
                { "id": "j1", "type": "bash", "status": "running", "description": "npm run dev", "command": "npm run dev" },
                { "id": "sub-1", "type": "subagent", "status": "running", "description": "reviewer" },
            ])
        );
    }
}
