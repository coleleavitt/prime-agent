//! `WorktreeCreate` / `WorktreeRemove` (Claude Code's VCS-agnostic worktree
//! hooks): when configured they replace git for `--worktree`,
//! `rlm.enter_worktree` and child isolation (`pa_core::worktree`).
//!
//! - `WorktreeCreate` gets the base fields plus `name`; it creates the
//!   directory and prints its absolute path as the last line of stdout. Exit 0
//!   with a path is success; anything else fails the creation (stderr is the
//!   reason). The first configured command runs.
//! - `WorktreeRemove` gets the base fields plus `worktree_path`; every
//!   configured command runs, and any non-zero exit fails the removal.
//!
//! Both read the `hooks` settings of the directory the worktree is made from
//! (the user's, then a trusted project's), and run there.

use std::path::{Path, PathBuf};

use pa_core::worktree::{WorktreeHookContext, WorktreeHooks};
use serde_json::{Value, json};

use crate::config::{HookCommand, HookEvent, HooksConfig};
use crate::run::{self, Outcome};

/// The hook provider `pa_core::worktree` consults.
#[derive(Debug, Default, Clone, Copy)]
pub struct WorktreeHookRunner;

/// Install [`WorktreeHookRunner`] as the process's worktree hook provider.
pub fn install() {
    pa_core::worktree::install_hooks(std::sync::Arc::new(WorktreeHookRunner));
}

impl WorktreeHooks for WorktreeHookRunner {
    fn create(&self, context: &WorktreeHookContext, name: &str) -> Option<Result<PathBuf, String>> {
        let config = HooksConfig::load(&context.cwd, &context.agent_dir);
        let hook = config
            .commands(HookEvent::WorktreeCreate, None)
            .into_iter()
            .next()?;
        let input = input(context, HookEvent::WorktreeCreate, "name", &json!(name));
        let outcome = run_blocking(vec![hook.clone()], context, input)
            .into_iter()
            .next()?;
        Some(created_path(&hook, &outcome))
    }

    fn remove(&self, context: &WorktreeHookContext, path: &Path) -> Option<Result<(), String>> {
        let config = HooksConfig::load(&context.cwd, &context.agent_dir);
        let hooks = config.commands(HookEvent::WorktreeRemove, None);
        if hooks.is_empty() {
            return None;
        }
        let input = input(
            context,
            HookEvent::WorktreeRemove,
            "worktree_path",
            &json!(path),
        );
        let failures: Vec<String> = hooks
            .iter()
            .zip(run_blocking(hooks.clone(), context, input))
            .filter(|(_, outcome)| outcome.code != Some(0))
            .map(|(hook, outcome)| failure(hook, &outcome))
            .collect();
        Some(if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        })
    }
}

/// Claude Code's base input plus the event's one field.
fn input(context: &WorktreeHookContext, event: HookEvent, key: &str, value: &Value) -> Value {
    json!({
        "session_id": context.session_id.clone().unwrap_or_default(),
        "transcript_path": "",
        "cwd": context.cwd,
        "hook_event_name": event.name(),
        "permission_mode": "default",
        key: value,
    })
}

/// A create hook's directory: the last non-empty stdout line of a zero exit.
fn created_path(hook: &HookCommand, outcome: &Outcome) -> Result<PathBuf, String> {
    if outcome.code != Some(0) {
        return Err(failure(hook, outcome));
    }
    outcome
        .stdout
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "hook {:?} printed no path (it must print the directory it created as the last \
                 line of its stdout)",
                hook.command
            )
        })
}

fn failure(hook: &HookCommand, outcome: &Outcome) -> String {
    let stderr = outcome.stderr.trim();
    match outcome.code {
        Some(code) if stderr.is_empty() => format!("hook {:?} exited {code}", hook.command),
        _ if stderr.is_empty() => format!("hook {:?} did not finish", hook.command),
        _ => stderr.to_string(),
    }
}

/// Run `hooks` to completion from synchronous code (the worktree helper is
/// blocking, and may be called on an async runtime's thread): a thread of
/// their own with a current-thread runtime.
fn run_blocking(
    hooks: Vec<HookCommand>,
    context: &WorktreeHookContext,
    input: Value,
) -> Vec<Outcome> {
    let cwd = context.cwd.clone();
    let session_id = context.session_id.clone().unwrap_or_default();
    let worker = std::thread::Builder::new()
        .name("pa-hooks-worktree".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            Ok::<_, String>(runtime.block_on(async {
                let mut outcomes = Vec::with_capacity(hooks.len());
                for hook in &hooks {
                    outcomes.push(run::run(hook, &cwd, &session_id, &input).await);
                }
                outcomes
            }))
        });
    match worker.map(std::thread::JoinHandle::join) {
        Ok(Ok(Ok(outcomes))) => outcomes,
        Ok(Ok(Err(error))) => vec![Outcome {
            code: None,
            stdout: String::new(),
            stderr: format!("cannot run worktree hooks: {error}"),
        }],
        Ok(Err(_)) | Err(_) => vec![Outcome {
            code: None,
            stdout: String::new(),
            stderr: "cannot run worktree hooks".to_string(),
        }],
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn context(temp: &Path, hooks: &Value) -> WorktreeHookContext {
        let agent_dir = temp.join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            json!({ "hooks": hooks }).to_string(),
        )
        .unwrap();
        let cwd = temp.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        WorktreeHookContext {
            cwd,
            agent_dir,
            session_id: Some("s1".to_string()),
        }
    }

    fn command(text: &str) -> Value {
        json!([{ "hooks": [{ "type": "command", "command": text }] }])
    }

    #[test]
    fn no_configured_hook_leaves_git_in_charge() {
        let temp = tempfile::tempdir().unwrap();
        let context = context(temp.path(), &json!({}));
        assert_eq!(WorktreeHookRunner.create(&context, "x"), None);
        assert_eq!(WorktreeHookRunner.remove(&context, temp.path()), None);
    }

    #[test]
    fn a_create_hook_makes_the_directory_and_prints_it() {
        let temp = tempfile::tempdir().unwrap();
        let made = temp.path().join("made");
        let created_input = temp.path().join("create-input.json");
        let removed_input = temp.path().join("remove-input.json");
        let script = format!(
            "cat > {input}; mkdir -p {made}/feat; echo creating; echo {made}/feat",
            input = created_input.display(),
            made = made.display()
        );
        let removal = format!(
            "cat > {input}; rm -r {made}/feat",
            input = removed_input.display(),
            made = made.display()
        );
        let context = context(
            temp.path(),
            &json!({
                "WorktreeCreate": command(&script),
                "WorktreeRemove": command(&removal),
            }),
        );
        let created = WorktreeHookRunner
            .create(&context, "feat")
            .unwrap()
            .unwrap();
        assert_eq!(created, made.join("feat"));
        assert!(created.is_dir());
        let read = |path: &Path| -> Value {
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let sent = read(&created_input);
        assert_eq!(sent["hook_event_name"], "WorktreeCreate");
        assert_eq!(sent["name"], "feat");
        assert_eq!(sent["session_id"], "s1");
        assert_eq!(WorktreeHookRunner.remove(&context, &created), Some(Ok(())));
        assert!(!created.exists());
        let sent = read(&removed_input);
        assert_eq!(sent["hook_event_name"], "WorktreeRemove");
        assert_eq!(sent["worktree_path"], json!(created));
    }

    #[test]
    fn a_failing_or_silent_create_hook_fails_the_creation() {
        let temp = tempfile::tempdir().unwrap();
        let failing = context(
            temp.path(),
            &json!({ "WorktreeCreate": command("echo no space left >&2; exit 1") }),
        );
        assert_eq!(
            WorktreeHookRunner.create(&failing, "x"),
            Some(Err("no space left".to_string()))
        );
        let silent = context(temp.path(), &json!({ "WorktreeCreate": command("true") }));
        let error = WorktreeHookRunner
            .create(&silent, "x")
            .unwrap()
            .unwrap_err();
        assert!(error.contains("printed no path"), "{error}");
    }
}
