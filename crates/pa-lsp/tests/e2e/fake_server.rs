//! A fake language server for a toy language (`.fake` files): `fn name`
//! lines define functions, `name(` calls them, `let x` lines are variables.
//! It speaks JSON-RPC over stdio like a real server and publishes a
//! diagnostic for every line containing `ERROR` (an error) or `WARN` (a
//! warning).
//!
//! Scripted by environment variables:
//! - `FAKE_LSP_STARTS_FILE`: append one line per start (tests count them).
//! - `FAKE_LSP_CRASH_ON`: exit with status 1 on a request of this method.
//! - `FAKE_LSP_CONTENT_MODIFIED`: answer the first request of this method
//!   with `ContentModified`.
//! - `FAKE_LSP_CONFIG_SECTION`: after `initialized`, ask the client for this
//!   `workspace/configuration` section; hovers show the answer.
//! - `FAKE_LSP_SILENT`: never publish diagnostics.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};

use serde_json::{Value, json};

struct Server {
    docs: BTreeMap<String, String>,
    config: Option<Value>,
    content_modified_sent: bool,
}

fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn send(message: &Value) {
    let body = message.to_string();
    let mut stdout = std::io::stdout().lock();
    let _ = write!(stdout, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = stdout.flush();
}

fn word_at(text: &str, line: usize, character: usize) -> Option<String> {
    let line: Vec<char> = text.lines().nth(line)?.chars().collect();
    let is_word = |c: &char| c.is_alphanumeric() || *c == '_';
    if character >= line.len() || !is_word(&line[character]) {
        return None;
    }
    let mut start = character;
    while start > 0 && is_word(&line[start - 1]) {
        start -= 1;
    }
    let mut end = character;
    while end < line.len() && is_word(&line[end]) {
        end += 1;
    }
    Some(line[start..end].iter().collect())
}

fn range(line: usize, start: usize, end: usize) -> Value {
    json!({"start": {"line": line, "character": start}, "end": {"line": line, "character": end}})
}

/// Each `fn` definition: (uri, line, name).
fn definitions(docs: &BTreeMap<String, String>) -> Vec<(String, usize, String)> {
    let mut found = Vec::new();
    for (uri, text) in docs {
        for (index, line) in text.lines().enumerate() {
            if let Some(name) = line.strip_prefix("fn ") {
                found.push((uri.clone(), index, name.trim().to_string()));
            }
        }
    }
    found
}

fn item(uri: &str, line: usize, name: &str) -> Value {
    json!({
        "name": name,
        "kind": 12,
        "uri": uri,
        "range": range(line, 0, 3 + name.len()),
        "selectionRange": range(line, 3, 3 + name.len()),
        "detail": "fn",
    })
}

/// The function whose body holds `line` of `text`.
fn enclosing(text: &str, line: usize) -> Option<(usize, String)> {
    text.lines()
        .enumerate()
        .take(line + 1)
        .filter_map(|(index, text)| {
            text.strip_prefix("fn ")
                .map(|name| (index, name.trim().to_string()))
        })
        .last()
}

impl Server {
    fn publish(&self, uri: &str, version: Option<i64>) {
        if std::env::var_os("FAKE_LSP_SILENT").is_some() {
            return;
        }
        let text = &self.docs[uri];
        let mut diagnostics = Vec::new();
        for (index, line) in text.lines().enumerate() {
            for (marker, severity) in [("ERROR", 1), ("WARN", 2)] {
                if let Some(column) = line.find(marker) {
                    diagnostics.push(json!({
                        "range": range(index, column, column + marker.len()),
                        "severity": severity,
                        "code": format!("E{severity}"),
                        "source": "fake",
                        "message": format!("found {marker}"),
                    }));
                }
            }
        }
        let mut params = json!({"uri": uri, "diagnostics": diagnostics});
        if let Some(version) = version {
            params["version"] = json!(version);
        }
        send(
            &json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params}),
        );
    }

    fn position(&self, params: &Value) -> Option<(String, String)> {
        let uri = params["textDocument"]["uri"].as_str()?.to_string();
        let line = usize::try_from(params["position"]["line"].as_u64()?).ok()?;
        let character = usize::try_from(params["position"]["character"].as_u64()?).ok()?;
        let word = word_at(self.docs.get(&uri)?, line, character)?;
        Some((uri, word))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per LSP method keeps the toy server readable in one place"
    )]
    fn answer(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        Ok(match method {
            "initialize" => json!({"capabilities": {
                "textDocumentSync": 1, "definitionProvider": true, "referencesProvider": true,
                "hoverProvider": true, "documentSymbolProvider": true, "workspaceSymbolProvider": true,
                "callHierarchyProvider": true, "implementationProvider": true,
            }}),
            "shutdown" => Value::Null,
            "textDocument/definition" | "textDocument/implementation" => {
                let Some((_, word)) = self.position(params) else {
                    return Ok(Value::Null);
                };
                let found: Vec<Value> = definitions(&self.docs)
                    .into_iter()
                    .filter(|(_, _, name)| *name == word)
                    .map(|(uri, line, name)| {
                        if method == "textDocument/implementation" {
                            json!({"targetUri": uri, "targetRange": range(line, 0, 3 + name.len()), "targetSelectionRange": range(line, 3, 3 + name.len())})
                        } else {
                            json!({"uri": uri, "range": range(line, 3, 3 + name.len())})
                        }
                    })
                    .collect();
                if found.is_empty() {
                    Value::Null
                } else {
                    Value::Array(found)
                }
            }
            "textDocument/references" => {
                let Some((_, word)) = self.position(params) else {
                    return Ok(json!([]));
                };
                let mut found = Vec::new();
                for (uri, text) in &self.docs {
                    for (index, line) in text.lines().enumerate() {
                        let mut from = 0;
                        while let Some(column) = line[from..].find(&word) {
                            let column = from + column;
                            found.push(json!({"uri": uri, "range": range(index, column, column + word.len())}));
                            from = column + word.len();
                        }
                    }
                }
                Value::Array(found)
            }
            "textDocument/hover" => {
                let Some((_, word)) = self.position(params) else {
                    return Ok(Value::Null);
                };
                let config = self
                    .config
                    .as_ref()
                    .map(|config| format!("\nconfig={config}"))
                    .unwrap_or_default();
                json!({
                    "contents": {"kind": "markdown", "value": format!("`{word}`: a fake symbol{config}")},
                    "range": {"start": params["position"], "end": params["position"]},
                })
            }
            "textDocument/documentSymbol" => {
                let uri = params["textDocument"]["uri"].as_str().unwrap_or_default();
                let text = self.docs.get(uri).cloned().unwrap_or_default();
                let mut symbols: Vec<Value> = Vec::new();
                for (index, line) in text.lines().enumerate() {
                    if let Some(name) = line.strip_prefix("fn ") {
                        let name = name.trim();
                        symbols.push(json!({
                            "name": name, "kind": 12, "detail": "fn()",
                            "range": range(index, 0, line.len()), "selectionRange": range(index, 3, line.len()),
                            "children": [],
                        }));
                    } else if let Some(variable) = line.trim().strip_prefix("let ") {
                        if let Some(Value::Array(children)) =
                            symbols.last_mut().map(|symbol| &mut symbol["children"])
                        {
                            children.push(json!({
                                "name": variable.trim(), "kind": 13,
                                "range": range(index, 2, line.len()), "selectionRange": range(index, 6, line.len()),
                            }));
                        }
                    }
                }
                Value::Array(symbols)
            }
            "workspace/symbol" => {
                let query = params["query"].as_str().unwrap_or_default();
                Value::Array(
                    definitions(&self.docs)
                        .into_iter()
                        .filter(|(_, _, name)| name.contains(query))
                        .map(|(uri, line, name)| {
                            json!({
                                "name": name, "kind": 12, "containerName": "toy",
                                "location": {"uri": uri, "range": range(line, 3, 3 + name.len())},
                            })
                        })
                        .collect(),
                )
            }
            "textDocument/prepareCallHierarchy" => {
                let Some((_, word)) = self.position(params) else {
                    return Ok(json!([]));
                };
                Value::Array(
                    definitions(&self.docs)
                        .into_iter()
                        .filter(|(_, _, name)| *name == word)
                        .map(|(uri, line, name)| item(&uri, line, &name))
                        .collect(),
                )
            }
            "callHierarchy/incomingCalls" => {
                let target = params["item"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let call = format!("{target}(");
                let mut calls: Vec<Value> = Vec::new();
                for (uri, text) in &self.docs {
                    for (index, line) in text.lines().enumerate() {
                        let Some(column) = line.find(&call) else {
                            continue;
                        };
                        let Some((def_line, caller)) = enclosing(text, index) else {
                            continue;
                        };
                        let site = range(index, column, column + target.len());
                        if let Some(existing) = calls
                            .iter_mut()
                            .find(|entry| entry["from"]["name"] == caller.as_str())
                        {
                            existing["fromRanges"]
                                .as_array_mut()
                                .expect("ranges")
                                .push(site);
                        } else {
                            calls.push(
                                json!({"from": item(uri, def_line, &caller), "fromRanges": [site]}),
                            );
                        }
                    }
                }
                Value::Array(calls)
            }
            "callHierarchy/outgoingCalls" => {
                let source = params["item"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let known = definitions(&self.docs);
                let mut calls = Vec::new();
                for (uri, text) in &self.docs {
                    for (index, line) in text.lines().enumerate() {
                        if enclosing(text, index).is_none_or(|(_, name)| name != source) {
                            continue;
                        }
                        for (target_uri, target_line, name) in &known {
                            if let Some(column) = line.find(&format!("{name}(")) {
                                let _ = uri;
                                calls.push(json!({
                                    "to": item(target_uri, *target_line, name),
                                    "fromRanges": [range(index, column, column + name.len())],
                                }));
                            }
                        }
                    }
                }
                Value::Array(calls)
            }
            _ => return Err((-32601, format!("unhandled {method}"))),
        })
    }

    fn notification(&mut self, method: &str, params: &Value) {
        match method {
            "exit" => std::process::exit(0),
            "textDocument/didOpen" => {
                let document = &params["textDocument"];
                let uri = document["uri"].as_str().unwrap_or_default().to_string();
                self.docs.insert(
                    uri.clone(),
                    document["text"].as_str().unwrap_or_default().to_string(),
                );
                self.publish(&uri, document["version"].as_i64());
            }
            "textDocument/didChange" => {
                let uri = params["textDocument"]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                if let Some(text) = params["contentChanges"][0]["text"].as_str() {
                    self.docs.insert(uri.clone(), text.to_string());
                }
                self.publish(&uri, params["textDocument"]["version"].as_i64());
            }
            "textDocument/didClose" => {
                if let Some(uri) = params["textDocument"]["uri"].as_str() {
                    self.docs.remove(uri);
                }
            }
            _ => {}
        }
    }
}

/// Serve stdin/stdout until `exit` or the end of input.
pub fn run() {
    if let Ok(path) = std::env::var("FAKE_LSP_STARTS_FILE") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("starts file");
        let _ = writeln!(file, "{}", std::process::id());
    }
    let crash_on = std::env::var("FAKE_LSP_CRASH_ON").ok();
    let content_modified = std::env::var("FAKE_LSP_CONTENT_MODIFIED").ok();
    let mut server = Server {
        docs: BTreeMap::new(),
        config: None,
        content_modified_sent: false,
    };
    let mut reader = BufReader::new(std::io::stdin().lock());
    let mut backlog: std::collections::VecDeque<Value> = std::collections::VecDeque::new();
    loop {
        let Some(message) = backlog.pop_front().or_else(|| read_message(&mut reader)) else {
            break;
        };
        // Ask for the configuration section and hold every other message
        // until the answer arrives, so later requests see it.
        if message["method"] == "initialized" {
            if let Ok(section) = std::env::var("FAKE_LSP_CONFIG_SECTION") {
                send(
                    &json!({"jsonrpc": "2.0", "id": "cfg-1", "method": "workspace/configuration", "params": {"items": [{"section": section}]}}),
                );
                while let Some(next) = read_message(&mut reader) {
                    if next["id"] == "cfg-1" && next.get("method").is_none() {
                        server.config = next["result"].get(0).cloned();
                        break;
                    }
                    backlog.push_back(next);
                }
            }
            continue;
        }
        let method = message["method"].as_str().map(str::to_string);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, message.get("id").cloned()) {
            (Some(method), Some(id)) => {
                if crash_on.as_deref() == Some(method.as_str()) {
                    eprintln!("fake server crashing on {method}");
                    std::process::exit(1);
                }
                if content_modified.as_deref() == Some(method.as_str())
                    && !server.content_modified_sent
                {
                    server.content_modified_sent = true;
                    send(
                        &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32801, "message": "content modified"}}),
                    );
                    continue;
                }
                let answer = match server.answer(&method, &params) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err((code, text)) => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}})
                    }
                };
                send(&answer);
            }
            (Some(method), None) => server.notification(&method, &params),
            (None, _) => {}
        }
    }
}
