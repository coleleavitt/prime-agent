//! A line-oriented guard oracle for differential runs against the Python
//! guards (`tests/corpus/differential.py`): each stdin line is one JSON
//! request `{guard, command, script, prefix, cwd, env, launchBypass}` and each
//! stdout line the Rust verdict `{"allowed": true}` or
//! `{"refused": <error class>, "message": <text>}`.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use pa_bash::{check, Allowances, GuardContext, GuardKind, Script};
use serde_json::{json, Value};

fn guard_named(name: &str) -> Option<GuardKind> {
    GuardKind::ALL.into_iter().find(|guard| {
        let key = match guard {
            GuardKind::DestructiveGit => "destructive_git",
            GuardKind::DestructiveChmod => "destructive_chmod",
            GuardKind::ForcePush => "force_push",
            GuardKind::SecretEcho => "secret_echo",
            GuardKind::PipeToShell => "pipe_to_shell",
            GuardKind::Sudo => "sudo",
        };
        key == name
    })
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let request: Value = serde_json::from_str(&line).expect("request parses");
        let guard = guard_named(request["guard"].as_str().expect("guard")).expect("known guard");
        let text = |key: &str| request.get(key).and_then(Value::as_str);
        let env: BTreeMap<String, String> = request["env"]
            .as_object()
            .expect("env")
            .iter()
            .map(|(name, value)| (name.clone(), value.as_str().unwrap_or_default().to_string()))
            .collect();
        let mut context = GuardContext::new(text("cwd").expect("cwd"), env);
        for bypassed in request["launchBypass"].as_array().into_iter().flatten() {
            if let Some(kind) = bypassed.as_str().and_then(guard_named) {
                context = context.with_launch_bypass(kind);
            }
        }
        let script_text = text("script").expect("script");
        let script = Script {
            command: text("command").unwrap_or(script_text),
            script: script_text,
            prefix: text("prefix"),
        };
        let allow = GuardKind::ALL
            .into_iter()
            .filter(|other| *other != guard)
            .fold(Allowances::none(), Allowances::allow);
        let verdict = match check(&script, &allow, &context) {
            Ok(()) => json!({"allowed": true}),
            Err(refusal) => {
                json!({"refused": refusal.guard.error_name(), "message": refusal.message})
            }
        };
        writeln!(stdout, "{verdict}").expect("stdout");
        stdout.flush().expect("stdout");
    }
}
