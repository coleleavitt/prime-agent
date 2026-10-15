//! Running one hook command and reading its answer, Claude Code's way:
//!
//! - exit 0: stdout may be a JSON object (`decision`, `reason`, `continue`,
//!   `stopReason`, `hookSpecificOutput.{permissionDecision,
//!   permissionDecisionReason, additionalContext}`); plain stdout is context
//!   for `SessionStart` / `UserPromptSubmit`;
//! - exit 2: blocks, with stderr as the reason;
//! - any other exit, a spawn failure or a timeout: a non-blocking error,
//!   reported and ignored.

use std::path::Path;
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::{HookCommand, HookEvent};

/// The most of a hook's stdout or stderr kept.
const MAX_OUTPUT: usize = 64 * 1024;

/// What one hook answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAnswer {
    /// No decision (exit 0 without one, or a non-blocking error).
    Pass { context: Option<String> },
    /// The hook allows explicitly (`permissionDecision: allow`).
    Allow { context: Option<String> },
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

fn shell(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut shell = tokio::process::Command::new("cmd");
        shell.arg("/C").arg(command);
        shell
    }
    #[cfg(not(windows))]
    {
        let mut shell = tokio::process::Command::new("sh");
        shell.arg("-c").arg(command);
        shell
    }
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

/// Run `hook` in `cwd` with `input` on its stdin. Never fails: a spawn
/// failure or a timeout is an outcome without a code, and its stderr says
/// why.
pub async fn run(hook: &HookCommand, cwd: &Path, session_id: &str, input: &Value) -> Outcome {
    let mut command = shell(&hook.command);
    command
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

/// Read one outcome as `event`'s answer; a non-blocking error is logged.
#[must_use]
pub fn answer(event: HookEvent, hook: &HookCommand, outcome: &Outcome) -> HookAnswer {
    match outcome.code {
        Some(0) => {}
        Some(2) => {
            let reason = outcome.stderr.trim();
            return HookAnswer::Block(if reason.is_empty() {
                format!("blocked by hook {:?}", hook.command)
            } else {
                reason.to_string()
            });
        }
        _ => {
            tracing::warn!(
                target: "pa_hooks",
                event = event.name(),
                command = hook.command.as_str(),
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
        // Plain stdout is context for the events that inject it.
        let context = matches!(event, HookEvent::SessionStart | HookEvent::UserPromptSubmit)
            .then(|| stdout.to_string())
            .filter(|text| !text.is_empty());
        return HookAnswer::Pass { context };
    };
    let specific = json.get("hookSpecificOutput").unwrap_or(&Value::Null);
    let context = text(specific, "additionalContext");
    if json.get("continue") == Some(&Value::Bool(false)) {
        return HookAnswer::Block(
            text(&json, "stopReason").unwrap_or_else(|| "stopped by a hook".to_string()),
        );
    }
    match specific.get("permissionDecision").and_then(Value::as_str) {
        Some("deny") => {
            return HookAnswer::Block(
                text(specific, "permissionDecisionReason")
                    .unwrap_or_else(|| "denied by a hook".to_string()),
            );
        }
        // No client confirmation surface: a policy hook must not fail open.
        Some("ask") => {
            let reason = text(specific, "permissionDecisionReason")
                .unwrap_or_else(|| "a hook asks for confirmation".to_string());
            return HookAnswer::Block(format!(
                "{reason} (needs confirmation, which this session cannot ask for)"
            ));
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
        HookCommand {
            command: command.to_string(),
            timeout: Duration::from_secs(5),
        }
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
            (pre, outcome(2, "", "no rm -rf\n"), HookAnswer::Block("no rm -rf".to_string())),
            (pre, outcome(1, "", "oops"), HookAnswer::Pass { context: None }),
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
                outcome(0, r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#, ""),
                HookAnswer::Block(
                    "a hook asks for confirmation (needs confirmation, which this session cannot ask for)"
                        .to_string(),
                ),
            ),
            (
                pre,
                outcome(0, r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#, ""),
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
            (pre, outcome(0, "plain text", ""), HookAnswer::Pass { context: None }),
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
                command: "sleep 30".to_string(),
                timeout: Duration::from_millis(200),
            },
            dir.path(),
            "s1",
            &json!({}),
        )
        .await;
        assert_eq!(slow.code, None);
        assert!(slow.stderr.contains("timed out"), "{slow:?}");
    }
}
