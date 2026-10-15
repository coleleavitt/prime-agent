//! What a plugin declares in its `metadata`: the tools it offers the model
//! and the slash commands it offers the user, checked once at load, and how
//! both are presented (the tool list a session's first prompt carries, the
//! `/wasm` listing).

use std::sync::Arc;

use serde_json::{Value, json};

use crate::bindings::prime_agent::plugin::types as wit;
use crate::instance::SessionPlugin;

/// A tool a plugin offers the model: a method of its `call` export whose
/// arguments the host validates against `parameters` first.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginTool {
    pub name: String,
    pub description: String,
    /// A JSON Schema for the arguments object.
    pub parameters: Value,
}

/// A slash command a plugin offers the user (`/wasm <plugin> <name>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
}

/// Whether `name` can name a tool or command: ASCII letters, digits, `_`,
/// `-` and `.`, so it reads unquoted in a listing and a command line.
fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn check_names<'a>(kind: &str, names: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for name in names {
        if !is_name(name) {
            return Err(format!(
                "{kind} name {name:?} must be ASCII letters, digits, `_`, `-` or `.`"
            ));
        }
        if !seen.insert(name) {
            return Err(format!("{kind} {name} is declared twice"));
        }
    }
    Ok(())
}

/// The declared tools, checked: valid unique names, and parameters that
/// are a JSON object schema (an empty string means no arguments).
pub(crate) fn tools(declared: Vec<wit::ToolSpec>) -> Result<Vec<PluginTool>, String> {
    check_names("tool", declared.iter().map(|tool| tool.name.as_str()))?;
    declared
        .into_iter()
        .map(|tool| {
            let parameters = if tool.parameters_json.trim().is_empty() {
                json!({ "type": "object", "properties": {} })
            } else {
                serde_json::from_str::<Value>(&tool.parameters_json)
                    .ok()
                    .filter(Value::is_object)
                    .ok_or_else(|| {
                        format!(
                            "tool {} parameters-json is not a JSON Schema object",
                            tool.name
                        )
                    })?
            };
            Ok(PluginTool {
                name: tool.name,
                description: tool.description.trim().to_string(),
                parameters,
            })
        })
        .collect()
}

/// The declared commands, checked: valid unique names.
pub(crate) fn commands(declared: Vec<wit::CommandSpec>) -> Result<Vec<PluginCommand>, String> {
    check_names(
        "command",
        declared.iter().map(|command| command.name.as_str()),
    )?;
    Ok(declared
        .into_iter()
        .map(|command| PluginCommand {
            name: command.name,
            description: command.description.trim().to_string(),
            argument_hint: command
                .argument_hint
                .map(|hint| hint.trim().to_string())
                .filter(|hint| !hint.is_empty()),
        })
        .collect())
}

/// A tool's signature as the model reads it: `search(query: string,
/// limit?: integer)`, from its schema's properties and `required` list.
#[must_use]
pub fn signature(tool: &PluginTool) -> String {
    let required: Vec<&str> = tool
        .parameters
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let arguments: Vec<String> = tool
        .parameters
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, schema)| {
            let kind = schema.get("type").and_then(Value::as_str).unwrap_or("any");
            let optional = if required.contains(&name.as_str()) {
                ""
            } else {
                "?"
            };
            format!("{name}{optional}: {kind}")
        })
        .collect();
    format!("{}({})", tool.name, arguments.join(", "))
}

/// The hidden context announcing the session's plugin tools to the model;
/// `None` when no plugin offers one.
pub(crate) fn tools_context(plugins: &[Arc<SessionPlugin>]) -> Option<String> {
    let lines: Vec<String> = plugins
        .iter()
        .flat_map(|plugin| {
            plugin.tools().iter().map(|tool| {
                let mut line = format!("- {}: {}", plugin.name(), signature(tool));
                if !tool.description.is_empty() {
                    line.push_str(" - ");
                    line.push_str(&tool.description);
                }
                line
            })
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "The user's WebAssembly plugins offer these tools. Call one from the Python REPL as \
         `await wasm_plugins.call(\"<plugin>\", \"<tool>\", **args)`; `await \
         wasm_plugins.tools()` lists them again.\n{}",
        lines.join("\n")
    ))
}

/// What `/wasm` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WasmCommand<'a> {
    /// List the session's plugins and their commands.
    List,
    /// Run `command` of `plugin` with `args` (the rest of the line).
    Run {
        plugin: &'a str,
        command: &'a str,
        args: &'a str,
    },
    /// A plugin without a command: show its commands.
    Plugin(&'a str),
}

/// Parse `/wasm` arguments: `[<plugin> [<command> [args...]]]`.
pub(crate) fn parse_wasm_command(args: &str) -> WasmCommand<'_> {
    let args = args.trim();
    let Some((plugin, rest)) = args
        .split_once(char::is_whitespace)
        .or((!args.is_empty()).then_some((args, "")))
    else {
        return WasmCommand::List;
    };
    let rest = rest.trim_start();
    if rest.is_empty() {
        return WasmCommand::Plugin(plugin);
    }
    let (command, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    WasmCommand::Run {
        plugin,
        command,
        args: args.trim(),
    }
}

/// The `/wasm` listing: each plugin, its description and commands.
pub(crate) fn listing<'a>(plugins: impl IntoIterator<Item = &'a Arc<SessionPlugin>>) -> String {
    let mut lines = Vec::new();
    for plugin in plugins {
        let mut header = plugin.name().to_string();
        if !plugin.description().is_empty() {
            header.push_str(" - ");
            header.push_str(plugin.description());
        }
        lines.push(header);
        if plugin.commands().is_empty() {
            lines.push("  (no commands)".to_string());
        }
        for command in plugin.commands() {
            let mut line = format!("  /wasm {} {}", plugin.name(), command.name);
            if let Some(hint) = &command.argument_hint {
                line.push(' ');
                line.push_str(hint);
            }
            if !command.description.is_empty() {
                line.push_str(" - ");
                line.push_str(&command.description);
            }
            lines.push(line);
        }
    }
    if lines.is_empty() {
        return "No WebAssembly plugins are loaded in this session.".to_string();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, parameters_json: &str) -> wit::ToolSpec {
        wit::ToolSpec {
            name: name.to_string(),
            description: " Search notes. ".to_string(),
            parameters_json: parameters_json.to_string(),
        }
    }

    #[test]
    fn declared_tools_are_checked() {
        let parsed = tools(vec![
            tool(
                "search",
                r#"{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"}},"required":["query"]}"#,
            ),
            tool("ping", ""),
        ])
        .unwrap();
        assert_eq!(parsed[0].description, "Search notes.");
        // Properties keep their declared order.
        assert_eq!(
            signature(&parsed[0]),
            "search(query: string, limit?: integer)"
        );
        assert_eq!(signature(&parsed[1]), "ping()");
        assert_eq!(
            tools(vec![tool("bad name", "")]).unwrap_err(),
            "tool name \"bad name\" must be ASCII letters, digits, `_`, `-` or `.`"
        );
        assert_eq!(
            tools(vec![tool("a", ""), tool("a", "")]).unwrap_err(),
            "tool a is declared twice"
        );
        assert_eq!(
            tools(vec![tool("a", "[1]")]).unwrap_err(),
            "tool a parameters-json is not a JSON Schema object"
        );
    }

    #[test]
    fn declared_commands_are_checked() {
        let command = |name: &str, hint: Option<&str>| wit::CommandSpec {
            name: name.to_string(),
            description: "d".to_string(),
            argument_hint: hint.map(str::to_string),
        };
        assert_eq!(
            commands(vec![
                command("greet", Some(" ")),
                command("go", Some("<x>"))
            ])
            .unwrap(),
            vec![
                PluginCommand {
                    name: "greet".to_string(),
                    description: "d".to_string(),
                    argument_hint: None,
                },
                PluginCommand {
                    name: "go".to_string(),
                    description: "d".to_string(),
                    argument_hint: Some("<x>".to_string()),
                },
            ]
        );
        assert!(commands(vec![command("", None)]).is_err());
    }

    #[test]
    fn wasm_arguments_parse() {
        assert_eq!(parse_wasm_command("  "), WasmCommand::List);
        assert_eq!(parse_wasm_command("notes"), WasmCommand::Plugin("notes"));
        assert_eq!(parse_wasm_command("notes  "), WasmCommand::Plugin("notes"));
        assert_eq!(
            parse_wasm_command("notes search"),
            WasmCommand::Run {
                plugin: "notes",
                command: "search",
                args: ""
            }
        );
        assert_eq!(
            parse_wasm_command("notes  search deploy  checklist "),
            WasmCommand::Run {
                plugin: "notes",
                command: "search",
                args: "deploy  checklist"
            }
        );
    }
}
