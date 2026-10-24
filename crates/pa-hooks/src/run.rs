//! Running one hook command and reading its answer, Claude Code's way:
//!
//! - exit 0: stdout may be a JSON object (`decision`, `reason`, `continue`,
//!   `stopReason`, `hookSpecificOutput.{permissionDecision,
//!   permissionDecisionReason, additionalContext}`); plain stdout is context
//!   for `SessionStart` / `UserPromptSubmit` (for `PreCompact`, added
//!   summary instructions); a `Stop` hook's `continue: false` lets the
//!   agent stop (it was stopping);
//! - exit 2: blocks, with stderr as the reason (for `Stop`: keeps the agent
//!   going, with stderr as the reason it reads);
//! - any other exit, a spawn failure or a timeout: a non-blocking error,
//!   reported and ignored.

use std::path::Path;
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::{CommandShell, HookCommand, HookEvent, HookKind, OnFailure};

/// The most of a hook's stdout or stderr kept.
const MAX_OUTPUT: usize = 64 * 1024;

/// What one hook answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAnswer {
    /// No decision (exit 0 without one, or a non-blocking error).
    Pass { context: Option<String> },
    /// The hook allows explicitly (`permissionDecision: allow`).
    Allow { context: Option<String> },
    /// The hook asks the user (`permissionDecision: ask`), with the reason
    /// shown to them.
    Ask(String),
    /// The hook refuses, with the reason the model sees.
    Block(String),
}

/// A command's raw outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// `None` when the command did not exit normally (killed, timed out,
    /// failed to spawn).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The process a `command` hook runs as: through its shell, or (`args`)
/// the executable itself.
fn process(hook: &HookCommand) -> tokio::process::Command {
    let shell = match &hook.kind {
        HookKind::Command {
            args: Some(args), ..
        } => {
            let mut process = tokio::process::Command::new(&hook.command);
            process.args(args);
            return process;
        }
        HookKind::Command { shell, .. } => *shell,
        HookKind::Http { .. } | HookKind::Prompt { .. } | HookKind::Agent { .. } => {
            CommandShell::Default
        }
    };
    let (program, flag) = match shell {
        CommandShell::Bash => (
            std::env::var("SHELL")
                .ok()
                .filter(|shell| !shell.is_empty())
                .unwrap_or_else(|| "bash".to_string()),
            "-c",
        ),
        CommandShell::PowerShell => ("pwsh".to_string(), "-Command"),
        #[cfg(windows)]
        CommandShell::Default => ("cmd".to_string(), "/C"),
        #[cfg(not(windows))]
        CommandShell::Default => ("sh".to_string(), "-c"),
    };
    let mut process = tokio::process::Command::new(program);
    process.arg(flag).arg(&hook.command);
    process
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(reader: Option<R>) -> String {
    let Some(reader) = reader else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = reader
        .take(u64::try_from(MAX_OUTPUT).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .await;
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Run `hook` with `input`: a `command` hook in `cwd` with the input on its
/// stdin, an `http` hook as a POST of it. Never fails: a spawn or request
/// failure or a timeout is an outcome without a code, and its stderr says
/// why. `prompt` and `agent` hooks need the session's model and run through
/// [`crate::model_hooks`]; here they are a failure.
pub async fn run(hook: &HookCommand, cwd: &Path, session_id: &str, input: &Value) -> Outcome {
    match &hook.kind {
        HookKind::Command { .. } => run_command(hook, cwd, session_id, input).await,
        HookKind::Http { .. } => crate::http::post(hook, input).await,
        HookKind::Prompt { .. } | HookKind::Agent { .. } => Outcome {
            code: None,
            stdout: String::new(),
            stderr: format!(
                "{} hook {:?} needs a session's model",
                hook.kind.type_name(),
                hook.command
            ),
        },
    }
}

async fn run_command(hook: &HookCommand, cwd: &Path, session_id: &str, input: &Value) -> Outcome {
    let mut command = process(hook);
    command
        .envs(hook.env.iter().map(|(name, value)| (name, value)))
        .current_dir(cwd)
        .env("CLAUDE_PROJECT_DIR", cwd)
        .env("PRIME_AGENT_PROJECT_DIR", cwd)
        .env("PRIME_AGENT_SESSION_ID", session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Outcome {
                code: None,
                stdout: String::new(),
                stderr: format!("cannot run hook {:?}: {error}", hook.command),
            };
        }
    };
    let stdin = child.stdin.take();
    let payload = input.to_string();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let run = async move {
        if let Some(mut stdin) = stdin {
            // A hook that ignores its input may close stdin first.
            let _ = stdin.write_all(payload.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
        let (stdout, stderr, status) =
            tokio::join!(read_capped(stdout), read_capped(stderr), child.wait());
        Outcome {
            code: status.ok().and_then(|status| status.code()),
            stdout,
            stderr,
        }
    };
    match tokio::time::timeout(hook.timeout, run).await {
        Ok(outcome) => outcome,
        // Dropping the future drops the child: `kill_on_drop` ends it.
        Err(_) => Outcome {
            code: None,
            stdout: String::new(),
            stderr: format!(
                "hook {:?} timed out after {}s",
                hook.command,
                hook.timeout.as_secs_f64()
            ),
        },
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// One hook's answer and the rest of what its JSON output says.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HookOutput {
    pub answer: HookAnswer,
    /// `systemMessage`: shown to the user, not the model.
    pub system_message: Option<String>,
    /// `hookSpecificOutput.updatedInput` (`PreToolUse`), or a
    /// `PermissionRequest` allow's `decision.updatedInput`.
    pub updated_input: Option<Value>,
    /// `hookSpecificOutput.watchPaths` (`SessionStart`, `CwdChanged`,
    /// `FileChanged`): files `FileChanged` watches.
    pub watch_paths: Vec<String>,
    /// `hookSpecificOutput.{action, content}` (`Elicitation`,
    /// `ElicitationResult`).
    pub elicitation: Option<pa_core::features::ElicitationResponse>,
}

impl Default for HookAnswer {
    fn default() -> Self {
        HookAnswer::Pass { context: None }
    }
}

/// Read one outcome as `event`'s [`HookOutput`].
#[must_use]
pub fn output(event: HookEvent, hook: &HookCommand, outcome: &Outcome) -> HookOutput {
    let answer = answer(event, hook, outcome);
    let json = (outcome.code == Some(0))
        .then(|| serde_json::from_str::<Value>(outcome.stdout.trim()).ok())
        .flatten()
        .filter(Value::is_object);
    let Some(json) = json else {
        return HookOutput {
            answer,
            ..HookOutput::default()
        };
    };
    let specific = json.get("hookSpecificOutput").unwrap_or(&Value::Null);
    let updated_input = match event {
        HookEvent::PermissionRequest => specific
            .get("decision")
            .filter(|decision| decision.get("behavior").and_then(Value::as_str) == Some("allow"))
            .and_then(|decision| decision.get("updatedInput")),
        _ => specific.get("updatedInput"),
    }
    .filter(|input| input.is_object())
    .cloned();
    let watch_paths = specific
        .get("watchPaths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let elicitation = specific
        .get("action")
        .and_then(Value::as_str)
        .and_then(pa_core::features::ElicitationAction::parse)
        .filter(|_| matches!(event, HookEvent::Elicitation | HookEvent::ElicitationResult))
        .map(|action| pa_core::features::ElicitationResponse {
            action,
            content: specific
                .get("content")
                .filter(|content| content.is_object())
                .cloned(),
        });
    HookOutput {
        answer,
        system_message: text(&json, "systemMessage"),
        updated_input,
        watch_paths,
        elicitation,
    }
}

/// Read one outcome as `event`'s answer; a non-blocking error is logged.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "one match over the hook-event kinds; splitting would scatter the outcome mapping"
)]
pub fn answer(event: HookEvent, hook: &HookCommand, outcome: &Outcome) -> HookAnswer {
    match outcome.code {
        Some(0) => {}
        Some(2) => {
            let reason = outcome.stderr.trim();
            return HookAnswer::Block(if reason.is_empty() {
                format!(
                    "blocked by hook {:?} from {}",
                    hook.command,
                    hook.source.label()
                )
            } else {
                reason.to_string()
            });
        }
        _ => {
            // `onFailure: "block"` counts a failure as exit code 2 where the
            // event can be blocked.
            if hook.on_failure == OnFailure::Block && event.failure_can_block() {
                let why = outcome.stderr.trim();
                return HookAnswer::Block(format!(
                    "hook {:?} from {} failed{}",
                    hook.command,
                    hook.source.label(),
                    if why.is_empty() {
                        String::new()
                    } else {
                        format!(": {why}")
                    }
                ));
            }
            tracing::warn!(
                target: "pa_hooks",
                event = event.name(),
                command = hook.command.as_str(),
                source = hook.source.label(),
                code = ?outcome.code,
                "hook failed (non-blocking): {}",
                outcome.stderr.trim()
            );
            return HookAnswer::Pass { context: None };
        }
    }
    let stdout = outcome.stdout.trim();
    let json = serde_json::from_str::<Value>(stdout)
        .ok()
        .filter(Value::is_object);
    let Some(json) = json else {
        // Plain stdout is context for the events that inject it (for
        // `PreCompact`, Claude Code's added summary instructions).
        let context = matches!(
            event,
            HookEvent::SessionStart | HookEvent::UserPromptSubmit | HookEvent::PreCompact
        )
        .then(|| stdout.to_string())
        .filter(|text| !text.is_empty());
        return HookAnswer::Pass { context };
    };
    let specific = json.get("hookSpecificOutput").unwrap_or(&Value::Null);
    let context = text(specific, "additionalContext");
    // A stop hook's `continue: false` stops the agent, which is stopping
    // anyway: only a `block` keeps it going.
    if matches!(event, HookEvent::Stop | HookEvent::SubagentStop)
        && json.get("continue") == Some(&Value::Bool(false))
    {
        return HookAnswer::Pass { context };
    }
    if json.get("continue") == Some(&Value::Bool(false)) {
        return HookAnswer::Block(
            text(&json, "stopReason").unwrap_or_else(|| "stopped by a hook".to_string()),
        );
    }
    // A `PermissionRequest` hook decides for the user.
    if let Some(decision) = specific
        .get("decision")
        .filter(|_| event == HookEvent::PermissionRequest)
    {
        match decision.get("behavior").and_then(Value::as_str) {
            Some("allow") => return HookAnswer::Allow { context },
            Some("deny") => {
                return HookAnswer::Block(
                    text(decision, "message").unwrap_or_else(|| "denied by a hook".to_string()),
                );
            }
            _ => {}
        }
    }
    match specific.get("permissionDecision").and_then(Value::as_str) {
        Some("deny") => {
            return HookAnswer::Block(
                text(specific, "permissionDecisionReason")
                    .unwrap_or_else(|| "denied by a hook".to_string()),
            );
        }
        // The session asks the user; without anyone to ask the action is
        // refused (a policy hook must not fail open).
        Some("ask") => {
            return HookAnswer::Ask(
                text(specific, "permissionDecisionReason")
                    .unwrap_or_else(|| "a hook asks for confirmation".to_string()),
            );
        }
        Some("allow") => return HookAnswer::Allow { context },
        _ => {}
    }
    match json.get("decision").and_then(Value::as_str) {
        Some("block") => HookAnswer::Block(
            text(&json, "reason").unwrap_or_else(|| "blocked by a hook".to_string()),
        ),
        Some("approve") => HookAnswer::Allow { context },
        _ => HookAnswer::Pass { context },
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    // Only the POSIX shell test builds JSON input.
    #[cfg(unix)]
    use serde_json::json;

    use super::*;

    fn hook(command: &str) -> HookCommand {
        HookCommand::shell(command, Duration::from_secs(5))
    }

    fn outcome(code: i32, stdout: &str, stderr: &str) -> Outcome {
        Outcome {
            code: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn answers_follow_claude_codes_contract() {
        let pre = HookEvent::PreToolUse;
        let cases = [
            (pre, outcome(0, "", ""), HookAnswer::Pass { context: None }),
            (
                pre,
                outcome(2, "", "no rm -rf\n"),
                HookAnswer::Block("no rm -rf".to_string()),
            ),
            (
                pre,
                outcome(1, "", "oops"),
                HookAnswer::Pass { context: None },
            ),
            (
                pre,
                outcome(
                    0,
                    r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"prod is off limits"}}"#,
                    "",
                ),
                HookAnswer::Block("prod is off limits".to_string()),
            ),
            (
                pre,
                outcome(
                    0,
                    r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#,
                    "",
                ),
                HookAnswer::Ask("a hook asks for confirmation".to_string()),
            ),
            (
                pre,
                outcome(
                    0,
                    r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"touches prod"}}"#,
                    "",
                ),
                HookAnswer::Ask("touches prod".to_string()),
            ),
            (
                pre,
                outcome(
                    0,
                    r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#,
                    "",
                ),
                HookAnswer::Allow { context: None },
            ),
            (
                pre,
                outcome(0, r#"{"decision":"block","reason":"legacy"}"#, ""),
                HookAnswer::Block("legacy".to_string()),
            ),
            (
                HookEvent::UserPromptSubmit,
                outcome(0, "remember: tabs\n", ""),
                HookAnswer::Pass {
                    context: Some("remember: tabs".to_string()),
                },
            ),
            (
                HookEvent::UserPromptSubmit,
                outcome(
                    0,
                    r#"{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"ctx"}}"#,
                    "",
                ),
                HookAnswer::Pass {
                    context: Some("ctx".to_string()),
                },
            ),
            (
                HookEvent::UserPromptSubmit,
                outcome(0, r#"{"continue":false,"stopReason":"maintenance"}"#, ""),
                HookAnswer::Block("maintenance".to_string()),
            ),
            (
                pre,
                outcome(0, "plain text", ""),
                HookAnswer::Pass { context: None },
            ),
        ];
        for (event, outcome, expected) in cases {
            assert_eq!(answer(event, &hook("h"), &outcome), expected, "{outcome:?}");
        }
    }

    #[test]
    fn stop_compaction_and_batch_answers_follow_claude_codes_contract() {
        let cases = [
            // A Stop block keeps the agent going; its `continue: false`
            // (stop now) does not.
            (
                HookEvent::Stop,
                outcome(0, r#"{"decision":"block","reason":"tests fail"}"#, ""),
                HookAnswer::Block("tests fail".to_string()),
            ),
            (
                HookEvent::Stop,
                outcome(2, "", "lint first"),
                HookAnswer::Block("lint first".to_string()),
            ),
            (
                HookEvent::Stop,
                outcome(0, r#"{"continue":false,"stopReason":"enough"}"#, ""),
                HookAnswer::Pass { context: None },
            ),
            (
                HookEvent::Stop,
                outcome(0, r#"{"decision":"approve"}"#, ""),
                HookAnswer::Allow { context: None },
            ),
            // PreCompact's plain stdout is added summary instructions.
            (
                HookEvent::PreCompact,
                outcome(0, "keep the API notes\n", ""),
                HookAnswer::Pass {
                    context: Some("keep the API notes".to_string()),
                },
            ),
            (
                HookEvent::PostToolBatch,
                outcome(
                    0,
                    r#"{"hookSpecificOutput":{"hookEventName":"PostToolBatch","additionalContext":"batch ctx"}}"#,
                    "",
                ),
                HookAnswer::Pass {
                    context: Some("batch ctx".to_string()),
                },
            ),
        ];
        for (event, outcome, expected) in cases {
            assert_eq!(answer(event, &hook("h"), &outcome), expected, "{outcome:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_reads_its_input_and_a_slow_one_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let echoed = run(
            &hook("cat; printf ' %s' \"$PRIME_AGENT_SESSION_ID\" >&2; exit 2"),
            dir.path(),
            "s1",
            &json!({ "hook_event_name": "PreToolUse" }),
        )
        .await;
        assert_eq!(
            echoed,
            outcome(2, r#"{"hook_event_name":"PreToolUse"}"#, " s1")
        );
        let slow = run(
            &HookCommand {
                timeout: Duration::from_millis(200),
                ..hook("sleep 30")
            },
            dir.path(),
            "s1",
            &json!({}),
        )
        .await;
        assert_eq!(slow.code, None);
        assert!(slow.stderr.contains("timed out"), "{slow:?}");
    }

    /// Claude Code's `env` reaches every hook command.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_runs_with_the_settings_environment() {
        let dir = tempfile::tempdir().unwrap();
        let ran = run(
            &HookCommand {
                env: std::sync::Arc::new(vec![("CC_SETTING".to_string(), "from-cc".to_string())]),
                ..hook("printf %s \"$CC_SETTING\"")
            },
            dir.path(),
            "s1",
            &json!({}),
        )
        .await;
        assert_eq!(ran, outcome(0, "from-cc", ""));
    }

    /// A refusal without a reason names the command and where it came from.
    #[test]
    fn a_silent_refusal_names_the_hooks_source() {
        let hook = HookCommand {
            source: crate::config::HookSource::ClaudeCode(
                pa_core::settings::claude_code::ClaudeScope::Project,
            ),
            ..hook("guard")
        };
        assert_eq!(
            answer(HookEvent::PreToolUse, &hook, &outcome(2, "", "")),
            HookAnswer::Block(
                "blocked by hook \"guard\" from Claude Code project settings".to_string()
            )
        );
    }
}
