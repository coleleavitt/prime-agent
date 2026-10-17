//! A local HTTP/1.1 server for the tests: each connection answers one
//! request through a route function and records what it received. No test
//! reaches the network.

#![allow(dead_code, reason = "each test binary uses part of the helpers")]

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One received request.
#[derive(Debug, Clone)]
pub struct Received {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Received {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// One response: status line, extra headers, body.
pub struct Reply {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn ok(content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            reason: "OK",
            headers: vec![("Content-Type".into(), content_type.into())],
            body: body.into(),
        }
    }

    pub fn status(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            reason: "Found",
            headers: vec![("Location".into(), location.into())],
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// A running server: its base URL and the requests it received.
pub struct Server {
    pub base: String,
    pub received: Arc<Mutex<Vec<Received>>>,
}

impl Server {
    pub fn requests(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Received> {
    let mut buffer = Vec::new();
    let mut chunk = vec![0; 16384];
    loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(split) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..split]).into_owned();
        let mut lines = head.lines();
        let mut start = lines.next()?.split_whitespace();
        let method = start.next()?.to_string();
        let target = start.next()?.to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_string(), value.trim().to_string()))
            })
            .collect();
        let length = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        while buffer.len() < split + 4 + length {
            let read = socket.read(&mut chunk).await.ok()?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body = String::from_utf8_lossy(&buffer[split + 4..]).into_owned();
        return Some(Received {
            method,
            target,
            headers,
            body,
        });
    }
}

/// Serve `route` on a fresh loopback port until the test ends.
pub async fn serve<F>(route: F) -> Server
where
    F: Fn(&Received) -> Reply + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let received = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&received);
    let route = Arc::new(route);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let recorder = Arc::clone(&recorder);
            let route = Arc::clone(&route);
            tokio::spawn(async move {
                let Some(request) = read_request(&mut socket).await else {
                    return;
                };
                let reply = route(&request);
                recorder.lock().unwrap().push(request);
                let mut head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.reason,
                    reply.body.len()
                );
                for (name, value) in &reply.headers {
                    head = format!("{head}{name}: {value}\r\n");
                }
                head.push_str("\r\n");
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&reply.body).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Server { base, received }
}

/// An Anthropic Messages SSE body from `events` (each a JSON event).
pub fn sse(events: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for event in events {
        body = format!(
            "{body}event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap_or_default()
        );
    }
    body
}

/// A streamed text answer.
pub fn text_answer(text: &str) -> String {
    sse(&[
        serde_json::json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":1,"output_tokens":1}}}),
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
        serde_json::json!({"type":"message_stop"}),
    ])
}

/// A test model on the Anthropic Messages API at `base_url`.
pub fn anthropic_model(base_url: &str) -> pa_types::ai::Model {
    serde_json::from_value(serde_json::json!({
        "id": "mock-haiku", "name": "Mock Haiku", "api": "anthropic-messages",
        "provider": "mock-anthropic", "baseUrl": base_url, "reasoning": false,
        "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 200_000, "maxTokens": 8192
    }))
    .unwrap()
}
