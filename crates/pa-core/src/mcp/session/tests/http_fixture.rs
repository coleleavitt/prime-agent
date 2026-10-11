//! A local fake streamable-HTTP MCP server on loopback: JSON responses
//! (no SSE), a session id, `GET` refused with 405 (no server stream), and
//! every request head recorded. Nothing leaves loopback.

use std::sync::{Arc, Mutex};

use pa_types::sync::MutexExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How the fake server answers.
pub(crate) struct Behavior {
    /// `tools/list` pages served in order (the last repeats): tool names and
    /// the next cursor.
    pub(crate) tool_pages: Vec<(Vec<String>, Option<String>)>,
    /// Answer every request with a 307 to this URL instead.
    pub(crate) redirect_to: Option<String>,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            tool_pages: vec![(vec!["http/raw.tool".to_string()], None)],
            redirect_to: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Recorded {
    headers: Vec<(String, String)>,
}

impl Recorded {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Default)]
struct State {
    requests: Vec<Recorded>,
    list_calls: usize,
    initializations: usize,
}

pub(crate) struct FakeHttpServer {
    port: u16,
    state: Arc<Mutex<State>>,
    _task: tokio::task::JoinHandle<()>,
}

impl FakeHttpServer {
    pub(crate) async fn start(behavior: Behavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let state: Arc<Mutex<State>> = Arc::default();
        let behavior = Arc::new(behavior);
        let task = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    tokio::spawn(serve(socket, Arc::clone(&state), Arc::clone(&behavior)));
                }
            })
        };
        Self {
            port,
            state,
            _task: task,
        }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub(crate) fn requests(&self) -> Vec<Recorded> {
        self.state.lock_or_recover().requests.clone()
    }

    pub(crate) fn initializations(&self) -> usize {
        self.state.lock_or_recover().initializations
    }
}

async fn serve(
    mut socket: tokio::net::TcpStream,
    state: Arc<Mutex<State>>,
    behavior: Arc<Behavior>,
) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let method = lines
        .next()
        .and_then(|line| line.split(' ').next())
        .unwrap_or_default()
        .to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(key, _)| key == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
        }
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.lock_or_recover().requests.push(Recorded { headers });
    let response = if let Some(target) = &behavior.redirect_to {
        format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    } else {
        match method.as_str() {
            "POST" => answer(&body, &state, &behavior),
            "DELETE" => {
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            }
            _ => {
                "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string()
            }
        }
    };
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

fn answer(body: &Value, state: &Mutex<State>, behavior: &Behavior) -> String {
    let Some(id) = body.get("id").cloned() else {
        return "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string();
    };
    let params = body.get("params").cloned().unwrap_or(Value::Null);
    let result = match body.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            state.lock_or_recover().initializations += 1;
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "fixture", "version": "1" },
            })
        }
        Some("tools/list") => {
            let mut state = state.lock_or_recover();
            let index = state.list_calls.min(behavior.tool_pages.len() - 1);
            state.list_calls += 1;
            let (names, next) = &behavior.tool_pages[index];
            let tools: Vec<Value> = names
                .iter()
                .map(|name| json!({ "name": name, "inputSchema": { "type": "object" } }))
                .collect();
            match next {
                Some(next) => json!({ "tools": tools, "nextCursor": next }),
                None => json!({ "tools": tools }),
            }
        }
        Some("tools/call") => {
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            json!({
                "content": [{ "type": "text", "text": arguments.to_string() }],
                "structuredContent": arguments,
            })
        }
        _ => json!({}),
    };
    let payload = json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: fixture-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
}
