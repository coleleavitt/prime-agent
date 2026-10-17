//! Claude Code's MCP scopes in `prime-agent mcp`: `--scope project` writes
//! the working directory's `.mcp.json` (Claude Code's entry shape: `${VAR}`
//! references where this product's settings name variables), `mcp list`
//! adds the `.mcp.json` and Claude Code servers with their approval, and
//! `mcp reset-project-choices` forgets this project's approvals and
//! rejections.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use pa_core::mcp::{
    DeclaredServers,
    MCP_CONFIG_MAX_BYTES,
    MCP_JSON_FILE,
    McpServerScope,
    ProjectApproval,
};
use serde_json::{Map, Value, json};

use crate::config::get_agent_dir;
use crate::mcp_command::McpServerConfig;

/// Where an `mcp add` / `mcp remove` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteScope {
    /// The user settings' `mcpServers` (the default).
    User,
    /// The working directory's `.mcp.json`.
    Project,
}

fn parse_scope(value: &str) -> Result<WriteScope> {
    match value {
        "user" => Ok(WriteScope::User),
        "project" => Ok(WriteScope::Project),
        "local" => bail!(
            "The local scope is Claude Code's own (~/.claude.json); use --scope user (Prime Agent \
             settings) or --scope project (.mcp.json)."
        ),
        other => bail!("Invalid scope: {other}. Use user or project."),
    }
}

/// Take `--scope <user|project>` (`-s`, `--scope=`) out of the options
/// before `--`; the rest pass through in order.
pub(crate) fn take_scope(args: &[String]) -> Result<(WriteScope, Vec<String>)> {
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut scope: Option<WriteScope> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let value = if index >= separator {
            None
        } else if arg == "--scope" || arg == "-s" {
            let value = args
                .get(index + 1)
                .filter(|_| index + 1 < separator)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("{arg} requires a value."))?;
            index += 1;
            Some(value.as_str())
        } else {
            arg.strip_prefix("--scope=")
        };
        match value {
            Some(value) => {
                if scope.is_some() {
                    bail!("Duplicate MCP option: --scope");
                }
                scope = Some(parse_scope(value)?);
            }
            None => rest.push(args[index].clone()),
        }
        index += 1;
    }
    Ok((scope.unwrap_or(WriteScope::User), rest))
}

fn current_dir() -> Result<PathBuf> {
    std::env::current_dir().context("resolving the current directory")
}

/// The `.mcp.json` document at `path`, or an empty one; never through a
/// symlink (Claude Code refuses those for a project-scope write).
fn read_document(path: &Path) -> Result<Map<String, Value>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Map::from_iter([(
                "mcpServers".to_string(),
                Value::Object(Map::new()),
            )]));
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "{}: .mcp.json is (or is under) a symlink \u{2014} refusing project-scope write",
            path.display()
        );
    }
    if !metadata.is_file() || metadata.len() > MCP_CONFIG_MAX_BYTES {
        bail!(
            "Can't read .mcp.json: it isn't a regular file or is larger than \
             {MCP_CONFIG_MAX_BYTES} bytes. Fix or remove it, then run the command again."
        );
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(Map::from_iter([(
            "mcpServers".to_string(),
            Value::Object(Map::new()),
        )]));
    }
    let malformed =
        || anyhow!(".mcp.json is malformed (not valid JSON, or mcpServers is not an object)");
    let Ok(Value::Object(mut document)) = serde_json::from_str::<Value>(&text) else {
        return Err(malformed());
    };
    match document.get("mcpServers") {
        None => {
            document.insert("mcpServers".to_string(), Value::Object(Map::new()));
        }
        Some(Value::Object(_)) => {}
        Some(_) => return Err(malformed()),
    }
    Ok(document)
}

fn write_document(path: &Path, document: &Map<String, Value>) -> Result<()> {
    let content = format!("{}\n", serde_json::to_string_pretty(document)?);
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let written = std::fs::write(&temp, content)
        .and_then(|()| std::fs::rename(&temp, path))
        .with_context(|| format!("Failed to write to .mcp.json: {}", path.display()));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

fn servers_mut(document: &mut Map<String, Value>) -> &mut Map<String, Value> {
    match document
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
    {
        Value::Object(servers) => servers,
        other => {
            *other = Value::Object(Map::new());
            let Value::Object(servers) = other else {
                unreachable!("just replaced with an object")
            };
            servers
        }
    }
}

/// A server as a `.mcp.json` entry: `--env CHILD=SOURCE` becomes
/// `"CHILD": "${SOURCE}"`, a bearer variable an `Authorization` header.
fn project_entry(config: &McpServerConfig) -> Result<Value> {
    match config {
        McpServerConfig::Stdio {
            command,
            args,
            cwd,
            env,
        } => {
            if cwd.is_some() {
                bail!(
                    "--cwd is not part of a .mcp.json entry; a project server starts in the \
                     session's directory."
                );
            }
            let mut entry = json!({
                "type": "stdio",
                "command": command,
                "args": args.clone().unwrap_or_default(),
            });
            if let Some(env) = env {
                entry["env"] = Value::Object(
                    env.iter()
                        .map(|(child, source)| {
                            (child.clone(), Value::String(format!("${{{}}}", source.env)))
                        })
                        .collect(),
                );
            }
            Ok(entry)
        }
        McpServerConfig::Http {
            url,
            bearer_token_env_var,
            oauth,
        } => {
            let mut entry = json!({ "type": "http", "url": url });
            if let Some(variable) = bearer_token_env_var {
                entry["headers"] = json!({ "Authorization": format!("Bearer ${{{variable}}}") });
            }
            if *oauth {
                entry["oauth"] = json!({});
            }
            Ok(entry)
        }
    }
}

/// `mcp add --scope project`: write the server into `./.mcp.json`.
pub(crate) fn add_project_server(
    name: &str,
    config: &McpServerConfig,
    force: bool,
) -> Result<String> {
    let path = current_dir()?.join(MCP_JSON_FILE);
    add_project_server_at(&path, name, config, force)
}

fn add_project_server_at(
    path: &Path,
    name: &str,
    config: &McpServerConfig,
    force: bool,
) -> Result<String> {
    let entry = project_entry(config)?;
    let mut document = read_document(path)?;
    let servers = servers_mut(&mut document);
    let replaced = servers.contains_key(name);
    if replaced && !force {
        bail!("MCP server \"{name}\" already exists in .mcp.json. Use --force to replace it.");
    }
    servers.insert(name.to_string(), entry);
    write_document(path, &document)?;
    Ok(format!(
        "{} MCP server \"{name}\" in {} (project scope: it runs once approved for this project).",
        if replaced { "Replaced" } else { "Added" },
        path.display()
    ))
}

/// `mcp remove --scope project`: drop the server from `./.mcp.json`.
pub(crate) fn remove_project_server(name: &str) -> Result<String> {
    let path = current_dir()?.join(MCP_JSON_FILE);
    remove_project_server_at(&path, name)
}

fn remove_project_server_at(path: &Path, name: &str) -> Result<String> {
    if !path.exists() {
        bail!("No MCP server named \"{name}\" in .mcp.json.");
    }
    let mut document = read_document(path)?;
    if servers_mut(&mut document).remove(name).is_none() {
        bail!("No MCP server named \"{name}\" in .mcp.json.");
    }
    write_document(path, &document)
        .with_context(|| format!("Failed to remove from .mcp.json: {}", path.display()))?;
    Ok(format!(
        "Removed MCP server \"{name}\" from {}.",
        path.display()
    ))
}

/// `mcp reset-project-choices`: forget this project's approvals and
/// rejections of its `.mcp.json` servers.
pub(crate) fn reset_project_choices() -> Result<String> {
    let workspace = pa_core::workspace_trust::workspace_key(&current_dir()?);
    let had = pa_core::mcp::reset_project_choices(&get_agent_dir(), &workspace)?;
    Ok(if had {
        "Project-scoped (.mcp.json) server approvals and rejections stored for this project have \
         been reset.\nYou will be prompted for approval next time you start Prime Agent here. \
         (enabledMcpjsonServers, disabledMcpjsonServers and enableAllProjectMcpServers in \
         settings files still apply.)"
            .to_string()
    } else {
        "No project-scoped (.mcp.json) server approvals or rejections are stored for this project."
            .to_string()
    })
}

/// `mcp list`'s lines for the servers declared outside the settings (the
/// `.mcp.json` files and, opted in, Claude Code's), then the problems
/// found loading them.
pub(crate) fn declared_server_lines() -> Vec<String> {
    let Ok(cwd) = current_dir() else {
        return Vec::new();
    };
    let agent_dir = get_agent_dir();
    let settings = pa_core::settings::SettingsManager::create(&cwd, &agent_dir);
    let declared = DeclaredServers::load(&cwd, &agent_dir, &settings);
    list_lines(&declared)
}

fn list_lines(declared: &DeclaredServers) -> Vec<String> {
    let mut lines: Vec<String> = declared
        .shown()
        .into_iter()
        .filter(|server| server.scope != McpServerScope::Settings)
        .map(|server| {
            let status = match server.approval {
                None | Some(ProjectApproval::Approved) => String::new(),
                Some(ProjectApproval::Pending) => {
                    ", pending approval: start prime-agent here to review it".to_string()
                }
                Some(ProjectApproval::Rejected) => {
                    ", rejected: `prime-agent mcp reset-project-choices` asks again".to_string()
                }
            };
            format!(
                "{}: {} ({} {}{status})",
                server.name,
                server.config.server_type(),
                server.scope.wire_name(),
                server.source.display()
            )
        })
        .collect();
    lines.extend(declared.issues().iter().map(|issue| {
        let location = issue.file.as_deref().unwrap_or("settings");
        match &issue.suggestion {
            Some(suggestion) => format!("Warning: {location}: {} ({suggestion})", issue.message),
            None => format!("Warning: {location}: {}", issue.message),
        }
    }));
    lines
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::mcp_command::EnvRef;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_scope_option_comes_out_of_the_options() {
        assert_eq!(
            take_scope(&args(&["x", "--scope", "project", "--url", "u"])).unwrap(),
            (WriteScope::Project, args(&["x", "--url", "u"]))
        );
        assert_eq!(
            take_scope(&args(&["x", "-s", "user", "--", "run", "--scope", "z"])).unwrap(),
            (WriteScope::User, args(&["x", "--", "run", "--scope", "z"]))
        );
        assert_eq!(
            take_scope(&args(&["x", "--scope=project"])).unwrap().0,
            WriteScope::Project
        );
        assert_eq!(take_scope(&args(&["x"])).unwrap().0, WriteScope::User);
        for (input, error) in [
            (vec!["x", "--scope"], "--scope requires a value."),
            (
                vec!["x", "--scope", "team"],
                "Invalid scope: team. Use user or project.",
            ),
            (
                vec!["x", "-s", "user", "--scope", "project"],
                "Duplicate MCP option: --scope",
            ),
        ] {
            assert_eq!(take_scope(&args(&input)).unwrap_err().to_string(), error);
        }
        assert!(
            take_scope(&args(&["x", "--scope", "local"]))
                .unwrap_err()
                .to_string()
                .starts_with("The local scope is Claude Code's own")
        );
    }

    #[test]
    fn project_add_and_remove_write_claude_codes_entry_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MCP_JSON_FILE);
        fs_write(
            &path,
            "{\"other\": 1, \"mcpServers\": {\"keep\": {\"command\": \"k\"}}}",
        );
        let stdio = McpServerConfig::Stdio {
            command: "npx".to_string(),
            args: Some(args(&["-y", "srv"])),
            cwd: None,
            env: Some(BTreeMap::from([(
                "TOKEN".to_string(),
                EnvRef {
                    env: "GITHUB_TOKEN".to_string(),
                },
            )])),
        };
        let http = McpServerConfig::Http {
            url: "https://mcp.example.com/".to_string(),
            bearer_token_env_var: Some("API_TOKEN".to_string()),
            oauth: false,
        };
        assert!(
            add_project_server_at(&path, "files", &stdio, false)
                .unwrap()
                .starts_with("Added MCP server \"files\" in ")
        );
        add_project_server_at(&path, "remote", &http, false).unwrap();
        assert_eq!(
            add_project_server_at(&path, "files", &stdio, false)
                .unwrap_err()
                .to_string(),
            "MCP server \"files\" already exists in .mcp.json. Use --force to replace it."
        );
        assert!(
            add_project_server_at(&path, "files", &stdio, true)
                .unwrap()
                .starts_with("Replaced")
        );
        let document: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            document,
            json!({
                "other": 1,
                "mcpServers": {
                    "keep": { "command": "k" },
                    "files": {
                        "type": "stdio",
                        "command": "npx",
                        "args": ["-y", "srv"],
                        "env": { "TOKEN": "${GITHUB_TOKEN}" }
                    },
                    "remote": {
                        "type": "http",
                        "url": "https://mcp.example.com/",
                        "headers": { "Authorization": "Bearer ${API_TOKEN}" }
                    }
                }
            })
        );
        remove_project_server_at(&path, "files").unwrap();
        assert_eq!(
            remove_project_server_at(&path, "files")
                .unwrap_err()
                .to_string(),
            "No MCP server named \"files\" in .mcp.json."
        );
        let with_cwd = McpServerConfig::Stdio {
            command: "x".to_string(),
            args: None,
            cwd: Some("/tmp".to_string()),
            env: None,
        };
        assert!(
            add_project_server_at(&path, "c", &with_cwd, false)
                .unwrap_err()
                .to_string()
                .starts_with("--cwd is not part of a .mcp.json entry")
        );
        fs_write(&path, "[]");
        assert_eq!(
            add_project_server_at(&path, "x", &http, false)
                .unwrap_err()
                .to_string(),
            ".mcp.json is malformed (not valid JSON, or mcpServers is not an object)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_mcp_json_is_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        fs_write(&real, "{\"mcpServers\": {}}");
        let path = dir.path().join(MCP_JSON_FILE);
        std::os::unix::fs::symlink(&real, &path).unwrap();
        let http = McpServerConfig::Http {
            url: "https://e.example.com/".to_string(),
            bearer_token_env_var: None,
            oauth: true,
        };
        assert!(
            add_project_server_at(&path, "x", &http, false)
                .unwrap_err()
                .to_string()
                .ends_with("refusing project-scope write")
        );
    }

    fn fs_write(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
    }
}
