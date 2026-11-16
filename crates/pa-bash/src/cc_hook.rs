//! The Claude Code `PreToolUse` hook (`prime-agent hook bash-guard`): the
//! same structural-grep and in-place-sed rules the kernel's `bash()` runs,
//! for Claude Code's `Bash` and `Grep` tools, with the Claude Code tools
//! named in the reasons. Only these two guards run: Claude Code's own
//! permission system governs the rest. A `# allow-grep` or `# allow-sed`
//! comment in a `Bash` command bypasses its guard; malformed input never
//! blocks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::context::GuardContext;
use crate::pipeline::{Allowances, check};
use crate::script::Script;
use crate::search::{SearchShape, structural_grep};
use crate::verdict::GuardKind;

/// The marker that bypasses the grep rule in a `Bash` command.
pub const ALLOW_GREP_MARKER: &str = "# allow-grep";
/// The marker that bypasses the sed rule in a `Bash` command.
pub const ALLOW_SED_MARKER: &str = "# allow-sed";

const GREP_REASON: &str = "Blocked: this searches project source for a code definition or call site with grep. Use the codegraph MCP and quote its file:line output: codegraph_node or codegraph_search for a definition, codegraph_callers and codegraph_callees for who calls what, codegraph_explore for how a feature is wired. grep only finds candidates, and the user has asked for the structural tools repeatedly.";
const SED_REASON: &str = "Blocked: `sed -i` edits a project source file in place. Use the Edit tool instead: it shows a diff and the edit can be rewound. In-place sed edits of source broke the build three to four times as often as edit-tool writes.";

/// The deny reason for a `PreToolUse` input, or `None` to let the tool run.
#[must_use]
pub fn claude_code_pre_tool_use(input: &Value) -> Option<String> {
    let cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    let tool_input = input.get("tool_input")?;
    match input.get("tool_name").and_then(Value::as_str)? {
        "Bash" => bash(tool_input.get("command")?.as_str()?, &cwd),
        "Grep" => grep_tool(tool_input, &cwd),
        _ => None,
    }
}

fn bash(command: &str, cwd: &Path) -> Option<String> {
    let mut allow = GuardKind::ALL
        .into_iter()
        .filter(|guard| !matches!(guard, GuardKind::Grep | GuardKind::Sed))
        .fold(Allowances::none(), Allowances::allow);
    if command.contains(ALLOW_GREP_MARKER) {
        allow = allow.allow(GuardKind::Grep);
    }
    if command.contains(ALLOW_SED_MARKER) {
        allow = allow.allow(GuardKind::Sed);
    }
    let context = GuardContext::new(cwd, std::env::vars().collect::<BTreeMap<_, _>>());
    let refusal = check(&Script::bare(command), &allow, &context).err()?;
    Some(match refusal.guard {
        GuardKind::Grep => format!(
            "{GREP_REASON} For a plain text search on purpose, add `{ALLOW_GREP_MARKER}` to the command."
        ),
        GuardKind::Sed => format!(
            "{SED_REASON} For a deliberate mechanical rewrite, add `{ALLOW_SED_MARKER}` to the command."
        ),
        _ => refusal.message,
    })
}

/// Claude Code's `Grep` tool: `pattern`, `path`, `glob`, `type`,
/// `output_mode` (`files_with_matches` by default, `content`, `count`).
fn grep_tool(tool_input: &Value, cwd: &Path) -> Option<String> {
    let field = |name: &str| tool_input.get(name).and_then(Value::as_str);
    let pattern = field("pattern")?;
    let mut args: Vec<String> = vec!["-e".to_string(), pattern.to_string()];
    match field("output_mode").unwrap_or("files_with_matches") {
        "content" => {}
        "count" => args.push("-c".to_string()),
        _ => args.push("-l".to_string()),
    }
    if let Some(kind) = field("type") {
        args.extend(["-t".to_string(), kind.to_string()]);
    }
    if let Some(glob) = field("glob") {
        args.extend(["-g".to_string(), glob.to_string()]);
    }
    if let Some(path) = field("path") {
        if Path::new(path).is_absolute() && !Path::new(path).starts_with(cwd) {
            return None;
        }
        args.push(path.to_string());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let found = structural_grep("rg", &args, false)?;
    let what = match found.shape {
        SearchShape::Definition => "a definition",
        SearchShape::CallSite => "call sites",
    };
    Some(format!(
        "{GREP_REASON} (This Grep looks up {what}: `{}`.) For a plain text search on purpose, use output_mode \"count\", or Bash with `{ALLOW_GREP_MARKER}`.",
        found.pattern
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn input(tool: &str, tool_input: &Value) -> Value {
        json!({"tool_name": tool, "tool_input": tool_input, "cwd": "/work/repo", "hook_event_name": "PreToolUse"})
    }

    #[test]
    fn bash_structural_greps_and_source_seds_are_denied() {
        let denied = claude_code_pre_tool_use(&input(
            "Bash",
            &json!({"command": "rg -n 'fn send_now' crates/"}),
        ));
        assert!(denied.unwrap().contains("codegraph_callers"));
        let sed = claude_code_pre_tool_use(&input(
            "Bash",
            &json!({"command": "sed -i 's/a/b/' src/lib.rs"}),
        ));
        assert!(sed.unwrap().contains("Edit tool"));
    }

    #[test]
    fn markers_text_searches_and_other_guards_pass() {
        for command in [
            "rg -n 'fn send_now' crates/ # allow-grep",
            "sed -i 's/a/b/' src/lib.rs # allow-sed",
            "rg -n TODO crates/",
            "cargo test 2>&1 | grep 'fn x'",
            "sudo make install",
            "git push --force origin main",
            "sleep 60",
        ] {
            assert_eq!(
                claude_code_pre_tool_use(&input("Bash", &json!({ "command": command }))),
                None,
                "{command}"
            );
        }
    }

    #[test]
    fn the_grep_tool_is_checked_in_content_mode() {
        let content =
            json!({"pattern": "fn\\s+send_now", "output_mode": "content", "path": "crates"});
        assert!(claude_code_pre_tool_use(&input("Grep", &content)).is_some());
        let typed = json!({"pattern": "isolate_hook\\(", "output_mode": "content", "type": "rust"});
        // The default mode lists files: "which file defines X" is still a lookup.
        let listed = json!({"pattern": "fn\\s+send_now", "path": "crates"});
        assert!(claude_code_pre_tool_use(&input("Grep", &listed)).is_some());
        assert!(claude_code_pre_tool_use(&input("Grep", &typed)).is_some());
        for allowed in [
            json!({"pattern": "isolate_hook\\(", "path": "crates"}),
            json!({"pattern": "fn\\s+send_now", "output_mode": "count"}),
            json!({"pattern": "TODO", "output_mode": "content"}),
            json!({"pattern": "fn\\s+x", "output_mode": "content", "path": "/usr/lib/rustlib"}),
        ] {
            assert_eq!(
                claude_code_pre_tool_use(&input("Grep", &allowed)),
                None,
                "{allowed}"
            );
        }
        assert_eq!(
            claude_code_pre_tool_use(&json!({"tool_name": "Bash"})),
            None,
            "malformed input passes"
        );
        assert_eq!(
            claude_code_pre_tool_use(&input("Read", &json!({"file_path": "x"}))),
            None
        );
    }
}
