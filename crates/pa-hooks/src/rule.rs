//! A hook's `if` condition: a Claude Code permission rule (`Bash`,
//! `Bash(git *)`, `Bash(npm run:*)`, `Edit(src/**)`, `mcp__github__*`) the
//! tool call must match for the hook to run, so a hook is not spawned for
//! calls it does not care about.
//!
//! The tool name matches exactly, or as a glob when it has a `*`. The
//! content, when given, is a glob (`*` matches any run of characters, `/`
//! included) over the call's subject: `tool_input.command` for `Bash`, else
//! the first of `file_path`, `path`, `url`, `pattern` and `code` that is a
//! string. A content ending in `:*` is Claude Code's legacy prefix form.

use serde_json::Value;

/// A parsed `if` rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRule {
    tool: String,
    content: Option<String>,
}

/// Whether `text` matches `pattern`, where `*` matches any run of
/// characters.
fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, rest) = (parts[0], &parts[1..]);
    let Some(mut remaining) = text.strip_prefix(first) else {
        return false;
    };
    let (last, middle) = rest.split_last().unwrap_or((&"", &[]));
    for part in middle {
        match remaining.find(part) {
            Some(index) => remaining = &remaining[index + part.len()..],
            None => return false,
        }
    }
    remaining.len() >= last.len() && remaining.ends_with(last)
}

impl ToolRule {
    /// Parse `Tool` or `Tool(content)`; `None` when it is not a rule.
    #[must_use]
    pub fn parse(rule: &str) -> Option<Self> {
        let rule = rule.trim();
        let (tool, content) = match rule.find('(') {
            Some(open) => {
                let content = rule[open + 1..].strip_suffix(')')?;
                (&rule[..open], Some(content.trim()))
            }
            None => (rule, None),
        };
        let tool = tool.trim();
        if tool.is_empty()
            || !tool
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '*'))
        {
            return None;
        }
        Some(Self {
            tool: tool.to_string(),
            content: content
                .filter(|content| !content.is_empty() && *content != "*")
                .map(str::to_string),
        })
    }

    /// Whether the call of `tool_name` with `input` matches.
    #[must_use]
    pub fn matches(&self, tool_name: &str, input: &Value) -> bool {
        if !glob(&self.tool, tool_name) {
            return false;
        }
        let Some(content) = &self.content else {
            return true;
        };
        let keys: &[&str] = if tool_name == "Bash" {
            &["command"]
        } else {
            &["file_path", "path", "url", "pattern", "code"]
        };
        let Some(subject) = keys
            .iter()
            .find_map(|key| input.get(*key).and_then(Value::as_str))
        else {
            return false;
        };
        match content.strip_suffix(":*") {
            Some(prefix) => subject == prefix || subject.starts_with(&format!("{prefix} ")),
            None => glob(content, subject),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rules_match_tool_names_and_subjects() {
        let bash = |command: &str| json!({ "command": command });
        let rule = ToolRule::parse("Bash(git *)").unwrap();
        assert!(rule.matches("Bash", &bash("git push origin main")));
        assert!(!rule.matches("Bash", &bash("rm -rf /")));
        assert!(!rule.matches("BashOutput", &bash("git status")));
        let prefix = ToolRule::parse("Bash(npm run:*)").unwrap();
        assert!(prefix.matches("Bash", &bash("npm run")));
        assert!(prefix.matches("Bash", &bash("npm run test")));
        assert!(!prefix.matches("Bash", &bash("npm runner")));
        let any = ToolRule::parse("Bash").unwrap();
        assert!(any.matches("Bash", &bash("anything")));
        assert!(ToolRule::parse("Bash(*)").unwrap().matches("Bash", &json!({})));
        let mcp = ToolRule::parse("mcp__github__*").unwrap();
        assert!(mcp.matches("mcp__github__search", &json!({})));
        assert!(!mcp.matches("mcp__gitlab__search", &json!({})));
        let edit = ToolRule::parse("Edit(src/*.rs)").unwrap();
        assert!(edit.matches("Edit", &json!({ "file_path": "src/lib.rs" })));
        assert!(!edit.matches("Edit", &json!({ "file_path": "docs/a.md" })));
        assert!(!edit.matches("Edit", &json!({})));
        assert_eq!(ToolRule::parse("Bash(git"), None);
        assert_eq!(ToolRule::parse("not a rule"), None);
        assert_eq!(ToolRule::parse(""), None);
    }

    #[test]
    fn globs_anchor_both_ends() {
        assert!(glob("a*c", "abc"));
        assert!(glob("a*c", "ac"));
        assert!(!glob("a*c", "abcd"));
        assert!(glob("*", ""));
        assert!(glob("a*b*c", "a-b-b-c"));
        assert!(!glob("ab*ba", "aba"));
    }
}
