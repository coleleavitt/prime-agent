//! `prime-agent hook bash-guard`: Claude Code's `PreToolUse` hook over the
//! kernel's grep and sed guards (`pa_bash::cc_hook`). Reads the hook input
//! from stdin; a deny prints Claude Code's `permissionDecision` JSON. Any
//! other outcome, a malformed input included, prints nothing and exits 0,
//! so a broken hook never blocks the user's tools.

use std::io::Read as _;

const USAGE: &str = "Usage: prime-agent hook bash-guard  (a Claude Code PreToolUse hook: reads the hook JSON on stdin)";

/// Run `hook <name>`; the process exit code.
pub(crate) fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("bash-guard") || args.len() != 1 {
        eprintln!("{USAGE}");
        return 2;
    }
    let mut text = String::new();
    if std::io::stdin().read_to_string(&mut text).is_err() {
        return 0;
    }
    let Ok(input) = serde_json::from_str::<serde_json::Value>(&text) else {
        return 0;
    };
    if let Some(reason) = pa_bash::cc_hook::claude_code_pre_tool_use(&input) {
        println!(
            "{}",
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }
            })
        );
    }
    0
}
