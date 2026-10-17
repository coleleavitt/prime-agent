//! The `lsp.*` host requests: one Claude Code LSP operation each, answered
//! with Claude Code's result text and the structured items beside it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::diagnostics::{FileDiagnostics, diagnostic_line, render_files};
use crate::format::{self, Direction, Formatted};
use crate::manager::{SessionServers, Synced, Unserved};
use crate::protocol::{self, Location, Severity, display_uri, path_to_uri};
use crate::server::LspServer;

/// The request types, in the order the kernel documents them.
pub const DEFINITION: &str = "lsp.definition";
pub const IMPLEMENTATION: &str = "lsp.implementation";
pub const REFERENCES: &str = "lsp.references";
pub const HOVER: &str = "lsp.hover";
pub const DOCUMENT_SYMBOLS: &str = "lsp.document_symbols";
pub const WORKSPACE_SYMBOLS: &str = "lsp.workspace_symbols";
pub const CALL_HIERARCHY: &str = "lsp.call_hierarchy";
pub const DIAGNOSTICS: &str = "lsp.diagnostics";
pub const SERVERS: &str = "lsp.servers";

/// Every request type this crate serves.
pub const REQUEST_TYPES: &[&str] = &[
    DEFINITION,
    IMPLEMENTATION,
    REFERENCES,
    HOVER,
    DOCUMENT_SYMBOLS,
    WORKSPACE_SYMBOLS,
    CALL_HIERARCHY,
    DIAGNOSTICS,
    SERVERS,
];

/// How long an explicit `rlm.lsp.diagnostics` read waits for a publish at
/// least (the boundary's wait when that is longer).
const DIAGNOSTICS_READ_WAIT: Duration = Duration::from_secs(5);
/// `git check-ignore` batch size and deadline (Claude Code's).
const IGNORE_BATCH: usize = 50;
const IGNORE_TIMEOUT: Duration = Duration::from_secs(5);

/// A file position from a payload: the absolute path and the 0-based
/// line and character.
struct At {
    path: PathBuf,
    line: u32,
    character: u32,
}

fn payload_path(data: &Value, cwd: &Path) -> anyhow::Result<PathBuf> {
    let path = data
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .context("path is required")?;
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(path),
        Ok(_) => bail!("Path is not a file: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("File does not exist: {}", path.display())
        }
        Err(error) => bail!("Cannot access file: {}. {error}", path.display()),
    }
}

fn one_based(data: &Value, key: &str) -> anyhow::Result<u32> {
    let value = data
        .get(key)
        .and_then(Value::as_u64)
        .filter(|value| *value >= 1)
        .with_context(|| {
            format!("{key} must be a positive integer (1-based, as shown in editors)")
        })?;
    u32::try_from(value - 1).with_context(|| format!("{key} is too large"))
}

fn position(data: &Value, cwd: &Path) -> anyhow::Result<At> {
    Ok(At {
        path: payload_path(data, cwd)?,
        line: one_based(data, "line")?,
        character: one_based(data, "character")?,
    })
}

fn reply(
    operation: &str,
    path: Option<&Path>,
    server: Option<&str>,
    formatted: Formatted,
) -> Value {
    let Formatted {
        text,
        items,
        result_count,
        file_count,
    } = formatted;
    json!({
        "operation": operation,
        "path": path.map(|path| path.display().to_string()),
        "server": server,
        "available": true,
        "text": text,
        "items": items,
        "result_count": result_count,
        "file_count": file_count,
    })
}

fn unavailable(operation: &str, path: &Path, unserved: &Unserved) -> Value {
    json!({
        "operation": operation,
        "path": path.display().to_string(),
        "server": Value::Null,
        "available": false,
        "text": unserved.text(),
        "items": [],
        "result_count": 0,
        "file_count": 0,
    })
}

/// The open file's server, or the reply that says why there is none.
async fn open<'a>(
    servers: &'a SessionServers,
    operation: &str,
    path: &Path,
) -> anyhow::Result<Result<(&'a std::sync::Arc<LspServer>, Synced), Value>> {
    match servers
        .sync(path, false)
        .await
        .map_err(anyhow::Error::msg)?
    {
        Ok(synced) => {
            let server = servers
                .server_for(path)
                .context("the file's server went away")?;
            Ok(Ok((server, synced)))
        }
        Err(unserved) => Ok(Err(unavailable(operation, path, &unserved))),
    }
}

fn text_document_position(uri: &str, at: &At) -> Value {
    json!({
        "textDocument": {"uri": uri},
        "position": {"line": at.line, "character": at.character},
    })
}

/// Drop locations in files git ignores (Claude Code's filter: generated and
/// vendored code is noise). Outside a git work tree nothing is dropped.
pub async fn without_ignored(locations: Vec<Location>, cwd: &Path) -> Vec<Location> {
    let mut paths: Vec<String> = Vec::new();
    for location in &locations {
        if let Some(path) = protocol::uri_to_path(&location.uri) {
            let path = path.display().to_string();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    if paths.is_empty() {
        return locations;
    }
    let mut ignored = std::collections::HashSet::new();
    for batch in paths.chunks(IGNORE_BATCH) {
        let output = tokio::process::Command::new("git")
            .arg("check-ignore")
            .args(batch)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let Ok(Ok(output)) = tokio::time::timeout(IGNORE_TIMEOUT, output).await else {
            continue;
        };
        if output.status.success() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let line = line.trim();
                if !line.is_empty() {
                    ignored.insert(line.to_string());
                }
            }
        }
    }
    if ignored.is_empty() {
        return locations;
    }
    locations
        .into_iter()
        .filter(|location| {
            protocol::uri_to_path(&location.uri)
                .is_none_or(|path| !ignored.contains(&path.display().to_string()))
        })
        .collect()
}

/// `lsp.definition` / `lsp.implementation` / `lsp.references`.
///
/// # Errors
///
/// An invalid payload, an unreadable file, or a server that could not
/// run or answer.
pub async fn locations(
    servers: &SessionServers,
    operation: &str,
    data: &Value,
) -> anyhow::Result<Value> {
    let at = position(data, servers.cwd())?;
    let (server, synced) = match open(servers, operation, &at.path).await? {
        Ok(opened) => opened,
        Err(reply) => return Ok(reply),
    };
    let mut params = text_document_position(&synced.uri, &at);
    let method = match operation {
        DEFINITION => "textDocument/definition",
        IMPLEMENTATION => "textDocument/implementation",
        _ => {
            let include = data
                .get("include_declaration")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            params["context"] = json!({"includeDeclaration": include});
            "textDocument/references"
        }
    };
    let answer = server
        .request(method, params)
        .await
        .map_err(anyhow::Error::msg)?;
    let found = without_ignored(protocol::locations(&answer), servers.cwd()).await;
    let formatted = if operation == REFERENCES {
        format::references(&found, servers.cwd())
    } else {
        format::definitions(&found, servers.cwd())
    };
    Ok(reply(
        operation,
        Some(&at.path),
        Some(server.name()),
        formatted,
    ))
}

/// `lsp.hover`.
///
/// # Errors
///
/// An invalid payload, an unreadable file, or a server that could not
/// run or answer.
pub async fn hover(servers: &SessionServers, data: &Value) -> anyhow::Result<Value> {
    let at = position(data, servers.cwd())?;
    let (server, synced) = match open(servers, HOVER, &at.path).await? {
        Ok(opened) => opened,
        Err(reply) => return Ok(reply),
    };
    let answer = server
        .request(
            "textDocument/hover",
            text_document_position(&synced.uri, &at),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let contents = (!answer.is_null())
        .then(|| {
            let text = protocol::hover_text(answer.get("contents").unwrap_or(&Value::Null));
            let start = answer
                .get("range")
                .and_then(|range| serde_json::from_value::<protocol::Range>(range.clone()).ok())
                .map(|range| (range.start.line, range.start.character));
            (text, start)
        })
        .filter(|(text, _)| !text.is_empty());
    Ok(reply(
        HOVER,
        Some(&at.path),
        Some(server.name()),
        format::hover(contents),
    ))
}

/// `lsp.document_symbols`.
///
/// # Errors
///
/// An invalid payload, an unreadable file, or a server that could not
/// run or answer.
pub async fn document_symbols(servers: &SessionServers, data: &Value) -> anyhow::Result<Value> {
    let path = payload_path(data, servers.cwd())?;
    let (server, synced) = match open(servers, DOCUMENT_SYMBOLS, &path).await? {
        Ok(opened) => opened,
        Err(reply) => return Ok(reply),
    };
    let answer = server
        .request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": synced.uri}}),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let formatted =
        format::document_symbols(&protocol::document_symbols(&answer), &path, servers.cwd());
    Ok(reply(
        DOCUMENT_SYMBOLS,
        Some(&path),
        Some(server.name()),
        formatted,
    ))
}

/// `lsp.workspace_symbols`: on the server of `path` when given, else on
/// every running server.
///
/// # Errors
///
/// An invalid payload, an unreadable file, or a server that could not
/// run or answer.
pub async fn workspace_symbols(servers: &SessionServers, data: &Value) -> anyhow::Result<Value> {
    let query = data
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (targets, path) = if data.get("path").and_then(Value::as_str).is_some() {
        let path = payload_path(data, servers.cwd())?;
        let (server, _) = match open(servers, WORKSPACE_SYMBOLS, &path).await? {
            Ok(opened) => opened,
            Err(reply) => return Ok(reply),
        };
        (vec![std::sync::Arc::clone(server)], Some(path))
    } else {
        let running = servers.running_servers().await;
        if running.is_empty() {
            if servers.is_empty() {
                bail!(
                    "No LSP servers are configured (the lspServers setting or a trusted project's .lsp.json)"
                );
            }
            bail!(
                "No language server is running yet: pass path= (a file of the language) to start its server"
            );
        }
        (running, None)
    };
    let mut symbols = Vec::new();
    let mut names = Vec::new();
    for server in &targets {
        let answer = server
            .request("workspace/symbol", json!({"query": query}))
            .await
            .map_err(anyhow::Error::msg)?;
        symbols.extend(protocol::symbol_informations(&answer));
        names.push(server.name().to_string());
    }
    let kept = without_ignored(
        symbols
            .iter()
            .map(|symbol| symbol.location.clone())
            .collect(),
        servers.cwd(),
    )
    .await;
    symbols.retain(|symbol| {
        kept.iter()
            .any(|location| location.uri == symbol.location.uri)
    });
    let formatted = format::workspace_symbols(&symbols, servers.cwd());
    Ok(reply(
        WORKSPACE_SYMBOLS,
        path.as_deref(),
        Some(&names.join(", ")),
        formatted,
    ))
}

/// `lsp.call_hierarchy` (`direction`: `incoming`, the default, `outgoing`
/// or `prepare`).
///
/// # Errors
///
/// An invalid payload, an unreadable file, or a server that could not
/// run or answer.
pub async fn call_hierarchy(servers: &SessionServers, data: &Value) -> anyhow::Result<Value> {
    let direction = match data
        .get("direction")
        .and_then(Value::as_str)
        .unwrap_or("incoming")
    {
        "incoming" => Some(Direction::Incoming),
        "outgoing" => Some(Direction::Outgoing),
        "prepare" => None,
        other => bail!("direction must be 'incoming', 'outgoing' or 'prepare', not {other:?}"),
    };
    let at = position(data, servers.cwd())?;
    let (server, synced) = match open(servers, CALL_HIERARCHY, &at.path).await? {
        Ok(opened) => opened,
        Err(reply) => return Ok(reply),
    };
    let answer = server
        .request(
            "textDocument/prepareCallHierarchy",
            text_document_position(&synced.uri, &at),
        )
        .await
        .map_err(anyhow::Error::msg)?;
    let items: Vec<protocol::CallHierarchyItem> = answer
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(protocol::CallHierarchyItem::parse)
        .collect();
    let formatted = match (direction, items.first()) {
        (None, _) => format::call_hierarchy_items(&items, servers.cwd()),
        (Some(_), None) => Formatted {
            text: "No call hierarchy item found at this position".to_string(),
            items: Vec::new(),
            result_count: 0,
            file_count: 0,
        },
        (Some(direction), Some(first)) => {
            let (method, field) = match direction {
                Direction::Incoming => ("callHierarchy/incomingCalls", "from"),
                Direction::Outgoing => ("callHierarchy/outgoingCalls", "to"),
            };
            let answer = server
                .request(method, json!({"item": first.raw}))
                .await
                .map_err(anyhow::Error::msg)?;
            format::calls(&protocol::calls(&answer, field), direction, servers.cwd())
        }
    };
    Ok(reply(
        CALL_HIERARCHY,
        Some(&at.path),
        Some(server.name()),
        formatted,
    ))
}

/// `lsp.diagnostics`: the file's current diagnostics (syncing it and
/// waiting for its server's publish when needed); what it shows is not
/// delivered again as a reminder.
///
/// # Errors
///
/// An invalid payload, an unreadable file, or a server that could not
/// run or answer.
pub async fn diagnostics(servers: &SessionServers, data: &Value) -> anyhow::Result<Value> {
    let path = payload_path(data, servers.cwd())?;
    let (server, synced) = match open(servers, DIAGNOSTICS, &path).await? {
        Ok(opened) => opened,
        Err(reply) => return Ok(reply),
    };
    let uri = path_to_uri(&path).context("not an absolute path")?;
    let waited = servers.read_diagnostics(&uri).is_none() || synced.sent;
    if waited {
        let wait = servers.diagnostics_wait().max(DIAGNOSTICS_READ_WAIT);
        let mut synced = synced;
        // Without a publish yet, wait for the first one.
        synced.sent = true;
        servers
            .wait_for_publishes(
                std::slice::from_ref(&synced),
                tokio::time::Instant::now() + wait,
            )
            .await;
    }
    let shown = display_uri(&uri, servers.cwd());
    let Some(found) = servers.read_diagnostics(&uri) else {
        return Ok(reply(
            DIAGNOSTICS,
            Some(&path),
            Some(server.name()),
            Formatted {
                text: format!("No diagnostics published for {shown} yet."),
                items: Vec::new(),
                result_count: 0,
                file_count: 0,
            },
        ));
    };
    let text = if found.is_empty() {
        format!("No diagnostics in {shown}.")
    } else {
        render_files(&[FileDiagnostics {
            uri: uri.clone(),
            diagnostics: found.clone(),
        }])
    };
    let items = found
        .iter()
        .map(|diagnostic| {
            json!({
                "path": path.display().to_string(),
                "line": diagnostic.range.start.line + 1,
                "character": diagnostic.range.start.character + 1,
                "severity": Severity::from_wire(diagnostic.severity).as_str(),
                "message": diagnostic.message,
                "code": diagnostic.code,
                "source": diagnostic.source,
                "text": diagnostic_line(diagnostic).trim_start(),
            })
        })
        .collect();
    Ok(reply(
        DIAGNOSTICS,
        Some(&path),
        Some(server.name()),
        Formatted {
            text,
            items,
            result_count: found.len(),
            file_count: usize::from(!found.is_empty()),
        },
    ))
}

/// `lsp.servers`: every configured server and what could not be configured.
pub async fn server_list(servers: &SessionServers) -> Value {
    let reports = servers.reports().await;
    let text = if reports.is_empty() {
        "No LSP servers are configured (the lspServers setting or a trusted project's .lsp.json)."
            .to_string()
    } else {
        reports
            .iter()
            .map(|report| {
                let mut line = format!(
                    "{} ({}) {} - {} [{}]",
                    report.name,
                    report.command,
                    report.status.as_str(),
                    report.extensions.join(" "),
                    report.source
                );
                if let Some(error) = &report.last_error {
                    line.push_str(" - ");
                    line.push_str(error);
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let errors = servers.config_errors();
    let text = if errors.is_empty() {
        text
    } else {
        format!(
            "{text}\nConfiguration errors:\n{}",
            errors
                .iter()
                .map(|error| format!("  {error}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    json!({
        "operation": SERVERS,
        "text": text,
        "servers": reports
            .iter()
            .map(|report| json!({
                "name": report.name,
                "command": report.command,
                "source": report.source,
                "extensions": report.extensions,
                "status": report.status.as_str(),
                "starts": report.starts,
                "last_error": report.last_error,
            }))
            .collect::<Vec<_>>(),
        "errors": errors,
    })
}
