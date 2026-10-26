//! Claude Code's MCP server entries: the `mcpServers` object of a
//! `.mcp.json` file and of `~/.claude.json`, parsed into this product's
//! [`McpServerConfig`] with Claude Code's validation and its error wording.
//! Variables expand as the entry loads (`${VAR}`, `${VAR:-default}`, see
//! [`super::expand`]): a stdio server's command, args and env values, an
//! HTTP server's URL and headers. A provider credential never expands into
//! a remote server's URL or headers.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Map, Value};

use super::expand::{Expansion, expand_env_vars};
use super::{IssueSeverity, McpConfigIssue};
use crate::mcp::{EnvValue, McpServerConfig};

/// The largest MCP config file read (Claude Code's limit).
pub const MCP_CONFIG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// How variables resolve while entries load.
pub(super) struct EnvLookup<'a> {
    /// A variable's value (the host environment under the kernel policy).
    pub(super) value: &'a dyn Fn(&str) -> Option<String>,
    /// Whether a variable is a credential that never reaches a remote server.
    pub(super) credential: &'a dyn Fn(&str) -> bool,
}

/// The servers of one `mcpServers` object, and what was wrong with it.
#[derive(Debug, Default)]
pub(super) struct ParsedServers {
    pub(super) servers: Vec<(String, McpServerConfig)>,
    pub(super) issues: Vec<McpConfigIssue>,
}

/// One config file's read.
#[derive(Debug)]
pub(super) enum FileRead {
    /// No file there (or a parent directory that cannot be searched).
    Absent,
    Read(Value),
    Failed(McpConfigIssue),
}

fn file_issue(path: &Path, message: String, suggestion: &str) -> McpConfigIssue {
    McpConfigIssue {
        file: Some(path.display().to_string()),
        path: String::new(),
        server: None,
        message,
        suggestion: Some(suggestion.to_string()),
        severity: IssueSeverity::Fatal,
    }
}

/// Read a JSON config file: a regular file (a symlink to one is fine) of
/// at most [`MCP_CONFIG_MAX_BYTES`].
pub(super) fn read_config_file(path: &Path) -> FileRead {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::NotADirectory
                    | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return FileRead::Absent;
        }
        Err(error) => {
            return FileRead::Failed(file_issue(
                path,
                format!("MCP config could not be read: {}: {error}", path.display()),
                "Check that the file is readable",
            ));
        }
    };
    if !metadata.is_file() || metadata.len() > MCP_CONFIG_MAX_BYTES {
        return FileRead::Failed(file_issue(
            path,
            format!(
                "MCP config is not a regular file or exceeds {MCP_CONFIG_MAX_BYTES} bytes: {}",
                path.display()
            ),
            "Check that the path is a plain JSON file (not a device, FIFO, or symlink to one)",
        ));
    }
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            return FileRead::Failed(file_issue(
                path,
                format!("MCP config could not be read: {}: {error}", path.display()),
                "Check that the file is readable",
            ));
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(document) => FileRead::Read(document),
        Err(_) => FileRead::Failed(file_issue(
            path,
            "MCP config is not a valid JSON".to_string(),
            "Fix the JSON syntax errors in the file",
        )),
    }
}

/// The servers of a `.mcp.json` document (`{"mcpServers": {...}}`).
pub(super) fn parse_mcp_json(document: &Value, file: &Path, env: &EnvLookup<'_>) -> ParsedServers {
    if let Some(Value::Object(servers)) = document.get("mcpServers") {
        return parse_servers(servers, Some(file), env);
    }
    let found_servers = document.get("servers").is_some();
    let (message, suggestion) = if found_servers && document.get("mcpServers").is_none() {
        (
            "Missing \"mcpServers\" \u{2014} found \"servers\" instead. Claude Code reads \
             MCP servers from the \"mcpServers\" key."
                .to_string(),
            Some(format!(
                "Rename the top-level \"servers\" key to \"mcpServers\" in {}",
                file.display()
            )),
        )
    } else {
        ("mcpServers: expected an object".to_string(), None)
    };
    ParsedServers {
        servers: Vec::new(),
        issues: vec![McpConfigIssue {
            file: Some(file.display().to_string()),
            path: "mcpServers".to_string(),
            server: None,
            message,
            suggestion,
            severity: IssueSeverity::Fatal,
        }],
    }
}

/// Parse an `mcpServers` object; entries that fail are skipped with a
/// warning, the rest load.
pub(super) fn parse_servers(
    servers: &Map<String, Value>,
    file: Option<&Path>,
    env: &EnvLookup<'_>,
) -> ParsedServers {
    let mut parsed = ParsedServers::default();
    for (name, entry) in servers {
        let mut warn = |message: String, suggestion: Option<String>| {
            parsed.issues.push(McpConfigIssue {
                file: file.map(|file| file.display().to_string()),
                path: format!("mcpServers.{name}"),
                server: Some(name.clone()),
                message,
                suggestion,
                severity: IssueSeverity::Warning,
            });
        };
        if let Some(config) = parse_entry(name, entry, env, &mut warn) {
            parsed.servers.push((name.clone(), config));
        }
    }
    parsed
}

/// One entry's raw (pre-expansion) shape.
enum RawEntry {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
        oauth: bool,
    },
}

/// A JSON type name in Claude Code's (zod's) wording.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn string_field(entry: &Value, key: &str, problems: &mut Vec<String>) -> Option<String> {
    match entry.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => {
            problems.push(format!(
                "{key}: expected string, received {}",
                type_name(other)
            ));
            None
        }
        None => {
            problems.push(format!("{key}: expected string, received undefined"));
            None
        }
    }
}

fn string_list(entry: &Value, key: &str, problems: &mut Vec<String>) -> Vec<String> {
    match entry.get(key) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match item {
                Value::String(text) => Some(text.clone()),
                other => {
                    problems.push(format!(
                        "{key}.{index}: expected string, received {}",
                        type_name(other)
                    ));
                    None
                }
            })
            .collect(),
        Some(other) => {
            problems.push(format!(
                "{key}: expected array, received {}",
                type_name(other)
            ));
            Vec::new()
        }
    }
}

fn string_map(entry: &Value, key: &str, problems: &mut Vec<String>) -> Vec<(String, String)> {
    match entry.get(key) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(name, item)| match item {
                Value::String(text) => Some((name.clone(), text.clone())),
                other => {
                    problems.push(format!(
                        "{key}.{name}: expected string, received {}",
                        type_name(other)
                    ));
                    None
                }
            })
            .collect(),
        Some(other) => {
            problems.push(format!(
                "{key}: expected record, received {}",
                type_name(other)
            ));
            Vec::new()
        }
    }
}

/// Values with leading or trailing whitespace (Claude Code warns: they are
/// used exactly as written).
fn whitespace_fields(raw: &RawEntry) -> Vec<String> {
    fn check(fields: &mut Vec<String>, field: String, value: &str) {
        if value != value.trim() {
            fields.push(field);
        }
    }
    let mut fields = Vec::new();
    match raw {
        RawEntry::Stdio { command, args, env } => {
            check(&mut fields, "command".to_string(), command);
            for (index, arg) in args.iter().enumerate() {
                check(&mut fields, format!("args[{index}]"), arg);
            }
            for (name, value) in env {
                check(&mut fields, format!("env name \"{name}\""), name);
                check(&mut fields, format!("env.{name}"), value);
            }
        }
        RawEntry::Http { url, headers, .. } => {
            check(&mut fields, "url".to_string(), url);
            for (name, value) in headers {
                check(&mut fields, format!("header name \"{name}\""), name);
                check(&mut fields, format!("headers.{name}"), value);
            }
        }
    }
    fields
}

/// Parse one entry; `None` when it is skipped (the reason went to `warn`).
fn parse_entry(
    name: &str,
    entry: &Value,
    env: &EnvLookup<'_>,
    warn: &mut dyn FnMut(String, Option<String>),
) -> Option<McpServerConfig> {
    let server_type = entry.get("type").and_then(Value::as_str).unwrap_or("stdio");
    let mut problems: Vec<String> = Vec::new();
    let raw = match server_type {
        "stdio" => {
            if !entry.is_object() {
                problems.push(format!(
                    "(root): expected object, received {}",
                    type_name(entry)
                ));
            }
            let command = string_field(entry, "command", &mut problems);
            let args = string_list(entry, "args", &mut problems);
            let env = string_map(entry, "env", &mut problems);
            command.map(|command| RawEntry::Stdio { command, args, env })
        }
        "http" | "streamable-http" => {
            let url = string_field(entry, "url", &mut problems);
            let headers = string_map(entry, "headers", &mut problems);
            let oauth = match entry.get("oauth") {
                None | Some(Value::Null | Value::Bool(false)) => false,
                Some(Value::Object(_) | Value::Bool(true)) => true,
                Some(other) => {
                    problems.push(format!(
                        "oauth: expected object, received {}",
                        type_name(other)
                    ));
                    false
                }
            };
            if entry.get("headersHelper").is_some() {
                warn(
                    format!(
                        "\"headersHelper\" of MCP server \"{name}\" is not supported by Prime \
                         Agent and was ignored"
                    ),
                    Some(format!(
                        "Set the headers of \"{name}\" directly (\"${{VAR}}\" references expand)"
                    )),
                );
            }
            url.map(|url| RawEntry::Http {
                url,
                headers,
                oauth,
            })
        }
        "sse" | "ws" | "sse-ide" | "ws-ide" | "sdk" | "claudeai-proxy" => {
            warn(
                format!(
                    "Skipped \u{2014} MCP server \"{name}\" uses the \"{server_type}\" transport, \
                     which Prime Agent does not support"
                ),
                Some(format!(
                    "Point \"{name}\" at a streamable HTTP endpoint (\"type\": \"http\") or a \
                     stdio command"
                )),
            );
            return None;
        }
        other => {
            warn(
                format!(
                    "Skipped \u{2014} unknown MCP server type \"{other}\" for server \"{name}\""
                ),
                Some("Valid types are: stdio, sse, http (or streamable-http), ws".to_string()),
            );
            return None;
        }
    };
    let raw = match raw {
        Some(raw) if problems.is_empty() => raw,
        _ => {
            if entry.get("type").is_none()
                && entry.get("url").is_some()
                && entry.get("command").is_none()
            {
                warn(
                    format!(
                        "Skipped \u{2014} MCP server \"{name}\" has a \"url\" but no \"type\"; add \
                         \"type\": \"http\" (or \"sse\" / \"ws\") to this entry"
                    ),
                    None,
                );
            } else {
                warn(
                    format!(
                        "Skipped \u{2014} invalid MCP server config for \"{name}\": {}",
                        problems.join("; ")
                    ),
                    None,
                );
            }
            return None;
        }
    };
    let whitespace = whitespace_fields(&raw);
    if !whitespace.is_empty() {
        warn(
            format!(
                "Leading or trailing whitespace in: {}",
                whitespace.join(", ")
            ),
            Some(format!(
                "Remove the whitespace from these values in the \"{name}\" entry \u{2014} they are \
                 used exactly as written"
            )),
        );
    }
    expand_entry(name, raw, env, warn)
}

/// Expand an entry's variables, reporting the missing and the blanked ones.
fn expand_entry(
    name: &str,
    raw: RawEntry,
    env: &EnvLookup<'_>,
    warn: &mut dyn FnMut(String, Option<String>),
) -> Option<McpServerConfig> {
    let mut missing: Vec<String> = Vec::new();
    let mut blanked: Vec<String> = Vec::new();
    let mut note = |expansion: Expansion| -> String {
        for variable in expansion.missing {
            if !missing.contains(&variable) {
                missing.push(variable);
            }
        }
        for variable in expansion.blanked {
            if !blanked.contains(&variable) {
                blanked.push(variable);
            }
        }
        expansion.text
    };
    let local = |text: &str| expand_env_vars(text, env.value, &|_| false);
    let remote = |text: &str| expand_env_vars(text, env.value, env.credential);
    let (config, url_emptied) = match raw {
        RawEntry::Stdio { command, args, env } => {
            let command = note(local(&command));
            let args: Vec<String> = args.iter().map(|arg| note(local(arg))).collect();
            let env: HashMap<String, EnvValue> = env
                .into_iter()
                .map(|(key, value)| (key, EnvValue::Literal(note(local(&value)))))
                .collect();
            (
                McpServerConfig::Stdio {
                    command,
                    args: (!args.is_empty()).then_some(args),
                    cwd: None,
                    env: (!env.is_empty()).then_some(env),
                    enabled: None,
                    enabled_tools: None,
                    disabled_tools: None,
                    startup_timeout_ms: None,
                    call_timeout_ms: None,
                },
                None,
            )
        }
        RawEntry::Http {
            url,
            headers,
            oauth,
        } => {
            let expanded_url = note(remote(&url));
            let emptied = (!url.trim().is_empty() && expanded_url.trim().is_empty()).then_some(url);
            let headers: HashMap<String, String> = headers
                .into_iter()
                .map(|(key, value)| (key, note(remote(&value))))
                .collect();
            (
                McpServerConfig::Http {
                    url: expanded_url,
                    headers: (!headers.is_empty()).then_some(headers),
                    bearer_token_env_var: None,
                    oauth: oauth.then_some(true),
                    enabled: None,
                    enabled_tools: None,
                    disabled_tools: None,
                    startup_timeout_ms: None,
                    call_timeout_ms: None,
                    client_id: None,
                    callback_port: None,
                },
                emptied,
            )
        }
    };
    if !missing.is_empty() {
        let list = missing.join(", ");
        warn(
            format!("Missing environment variables: {list}"),
            Some(format!("Set the following environment variables: {list}")),
        );
    }
    if !blanked.is_empty() {
        warn(
            format!(
                "MCP server config references credential variable(s) that are never expanded \
                 toward a remote server: {} (read as empty)",
                blanked.join(", ")
            ),
            None,
        );
    }
    if let Some(url) = url_emptied {
        warn(
            format!(
                "Skipped \u{2014} 'url' \"{url}\" of MCP server \"{name}\" expanded to an empty \
                 string"
            ),
            Some(
                "Set the referenced environment variable, or update the server's config"
                    .to_string(),
            ),
        );
        return None;
    }
    Some(config)
}
