//! What the session's surfaces ask to do, as permission actions: the
//! kernel's host requests (its `bash()`, the `edit` skill's `file.edit`,
//! `web.*`, MCP calls) and the model's tool calls. Everything else the
//! kernel does is Python the rules cannot see.

use std::path::{Path, PathBuf};

use pa_core::features::{HostRequestObservation, ToolCallObservation};
use serde_json::Value;

use crate::decide::Action;
use crate::paths::{PathAnchors, absolute};

/// The host request the bundled `edit` skill sends before it writes a file
/// (`{"path": <absolute path>}`).
pub const FILE_EDIT_REQUEST: &str = "file.edit";

/// The host request a reader may send before it reads a file
/// (`{"path": <absolute path>}`).
pub const FILE_READ_REQUEST: &str = "file.read";

fn text<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    data.get(key).and_then(Value::as_str)
}

fn path_of(data: &Value, anchors: &PathAnchors) -> Option<PathBuf> {
    text(data, "path")
        .or_else(|| text(data, "file_path"))
        .filter(|path| !path.is_empty())
        .map(|path| absolute(path, anchors))
}

/// The action a kernel host request asks for; `None` for the requests the
/// rules do not govern.
#[must_use]
pub fn from_host_request(
    request: &HostRequestObservation,
    anchors: &PathAnchors,
) -> Option<Action> {
    let data = &request.data;
    match request.request_type.as_str() {
        "bash.run" => Some(Action::Bash {
            command: text(data, "command").unwrap_or_default().to_string(),
            cwd: text(data, "cwd")
                .filter(|cwd| !cwd.is_empty())
                .map_or_else(|| anchors.cwd.clone(), |cwd| absolute(cwd, anchors)),
        }),
        FILE_EDIT_REQUEST => path_of(data, anchors).map(|path| Action::Edit { path }),
        FILE_READ_REQUEST => path_of(data, anchors).map(|path| Action::Read { path }),
        "mcp.session.call_tool" | "mcp.integration.call_tool" => Some(Action::Mcp {
            server: text(data, "server").unwrap_or_default().to_string(),
            tool: text(data, "tool").unwrap_or_default().to_string(),
        }),
        other if other.starts_with("web.") => {
            if let Some(url) = text(data, "url") {
                Some(Action::WebFetch {
                    url: url.to_string(),
                })
            } else {
                text(data, "query").map(|query| Action::WebSearch {
                    query: query.to_string(),
                })
            }
        }
        _ => None,
    }
}

/// The action a model tool call asks for: the legacy `bash` and `edit`
/// tools as `Bash` and `Edit`, any other tool by its name.
#[must_use]
pub fn from_tool_call(call: &ToolCallObservation, anchors: &PathAnchors) -> Action {
    match call.tool_name.as_str() {
        "bash" => Action::Bash {
            command: text(&call.args, "command").unwrap_or_default().to_string(),
            cwd: anchors.cwd.clone(),
        },
        "edit" => match path_of(&call.args, anchors) {
            Some(path) => Action::Edit { path },
            None => Action::Tool {
                name: call.tool_name.clone(),
            },
        },
        name => Action::Tool {
            name: name.to_string(),
        },
    }
}

/// The action a rule-shaped probe names (`/permissions check
/// Bash(git push)`): its content as the command, path, URL or query.
#[must_use]
pub fn from_probe(rule: &crate::rule::RuleValue, anchors: &PathAnchors) -> Action {
    let content = rule.content.clone().unwrap_or_default();
    if let Some((server, tool)) = rule.mcp() {
        return Action::Mcp {
            server: server.to_string(),
            tool: tool.unwrap_or("*").to_string(),
        };
    }
    match rule.tool.as_str() {
        "Bash" => Action::Bash {
            command: content,
            cwd: anchors.cwd.clone(),
        },
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => Action::Edit {
            path: absolute(&content, anchors),
        },
        "Read" | "Glob" | "Grep" => Action::Read {
            path: absolute(&content, anchors),
        },
        "WebFetch" => Action::WebFetch { url: content },
        "WebSearch" => Action::WebSearch { query: content },
        name => Action::Tool {
            name: name.to_string(),
        },
    }
}

/// How an action is put to the user.
#[must_use]
pub fn describe(action: &Action, cwd: &Path) -> String {
    const MAX: usize = 600;
    let clip = |text: &str| match text.char_indices().nth(MAX) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_string(),
    };
    match action {
        Action::Bash { command, .. } => format!("Run `{}`", clip(command)),
        Action::Edit { path } => {
            format!("Edit {}", path.strip_prefix(cwd).unwrap_or(path).display())
        }
        Action::Read { path } => {
            format!("Read {}", path.strip_prefix(cwd).unwrap_or(path).display())
        }
        Action::WebFetch { url } => format!("Fetch {}", clip(url)),
        Action::WebSearch { query } => format!("Search the web for {}", clip(query)),
        Action::Mcp { server, tool } => format!("Call the MCP tool {tool} of {server}"),
        Action::Tool { name } => format!("Run tool `{name}`"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn anchors() -> PathAnchors {
        PathAnchors {
            cwd: PathBuf::from("/work"),
            home: Some(PathBuf::from("/home/me")),
        }
    }

    fn request(request_type: &str, data: Value) -> HostRequestObservation {
        HostRequestObservation {
            request_type: request_type.to_string(),
            data,
        }
    }

    #[test]
    fn host_requests_map_onto_claude_codes_tools() {
        let anchors = anchors();
        assert_eq!(
            from_host_request(
                &request(
                    "bash.run",
                    json!({ "command": "ls", "cwd": "/work/sub", "env": {} })
                ),
                &anchors
            ),
            Some(Action::Bash {
                command: "ls".to_string(),
                cwd: PathBuf::from("/work/sub"),
            })
        );
        assert_eq!(
            from_host_request(
                &request("file.edit", json!({ "path": "src/a.rs" })),
                &anchors
            ),
            Some(Action::Edit {
                path: PathBuf::from("/work/src/a.rs"),
            })
        );
        assert_eq!(
            from_host_request(
                &request(
                    "mcp.session.call_tool",
                    json!({ "server": "github", "tool": "get_issue" })
                ),
                &anchors
            ),
            Some(Action::Mcp {
                server: "github".to_string(),
                tool: "get_issue".to_string(),
            })
        );
        assert_eq!(
            from_host_request(
                &request("web.fetch", json!({ "url": "https://a.dev/x" })),
                &anchors
            ),
            Some(Action::WebFetch {
                url: "https://a.dev/x".to_string(),
            })
        );
        assert_eq!(
            from_host_request(&request("web.search", json!({ "query": "rust" })), &anchors),
            Some(Action::WebSearch {
                query: "rust".to_string(),
            })
        );
        assert_eq!(
            from_host_request(&request("rlm.run", json!({})), &anchors),
            None
        );
        assert_eq!(
            from_host_request(&request("file.edit", json!({})), &anchors),
            None
        );
    }

    #[test]
    fn probes_name_an_action_by_rule_shape() {
        let anchors = anchors();
        let probe = |text: &str| from_probe(&crate::rule::RuleValue::parse(text), &anchors);
        assert_eq!(
            probe("Bash(git push)"),
            Action::Bash {
                command: "git push".to_string(),
                cwd: PathBuf::from("/work"),
            }
        );
        assert_eq!(
            probe("Edit(./a.txt)"),
            Action::Edit {
                path: PathBuf::from("/work/a.txt"),
            }
        );
        assert_eq!(
            probe("mcp__github__get_issue"),
            Action::Mcp {
                server: "github".to_string(),
                tool: "get_issue".to_string(),
            }
        );
    }
}
