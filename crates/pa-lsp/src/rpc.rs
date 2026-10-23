//! A JSON-RPC 2.0 client over a language server's stdio: `Content-Length`
//! framed messages, requests matched to responses by id, the server's own
//! requests answered by a handler, its notifications handed to another.
//!
//! The process is spawned with `kill_on_drop`, so dropping the client ends
//! it; [`RpcClient::shutdown`] is the graceful path (`shutdown`, `exit`,
//! then a bounded wait before the kill).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// The most stderr lines kept for a crash report.
const STDERR_TAIL_LINES: usize = 20;
/// The largest message accepted from a server.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// An error answer from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// Why a request got no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// The server answered with an error.
    Rpc(RpcError),
    /// No answer within the timeout.
    Timeout(Duration),
    /// The connection closed (the server exited) before an answer.
    Closed,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(error) => write!(f, "{} (code {})", error.message, error.code),
            Self::Timeout(timeout) => write!(f, "no response after {}ms", timeout.as_millis()),
            Self::Closed => f.write_str("the server exited"),
        }
    }
}

/// Handles one notification from the server (`method`, `params`).
pub type NotificationHandler = Arc<dyn Fn(&str, Value) + Send + Sync>;
/// Answers one request from the server (`method`, `params`).
pub type RequestHandler = Arc<dyn Fn(&str, Value) -> Result<Value, RpcError> + Send + Sync>;

/// What to spawn.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// One connection to a running server process.
pub struct RpcClient {
    writer: mpsc::UnboundedSender<Vec<u8>>,
    pending: Pending,
    next_id: AtomicI64,
    closed: CancellationToken,
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient")
            .field("closed", &self.closed.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// One framed message.
fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl RpcClient {
    /// Spawn the server and start its reader, writer and stderr tasks (on
    /// the current Tokio runtime).
    ///
    /// # Errors
    ///
    /// The process could not be spawned.
    pub fn spawn(
        spec: &SpawnSpec,
        on_notification: NotificationHandler,
        on_request: RequestHandler,
    ) -> std::io::Result<Arc<Self>> {
        let mut command = tokio::process::Command::new(&spec.command);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;
        let stderr = child.stderr.take();
        let (writer, mut outgoing) = mpsc::unbounded_channel::<Vec<u8>>();
        let client = Arc::new(Self {
            writer,
            pending: Pending::default(),
            next_id: AtomicI64::new(1),
            closed: CancellationToken::new(),
            child: tokio::sync::Mutex::new(Some(child)),
            stderr_tail: Arc::default(),
        });
        let closed = client.closed.clone();
        tokio::spawn(async move {
            let mut stdin = stdin;
            loop {
                tokio::select! {
                    () = closed.cancelled() => break,
                    message = outgoing.recv() => {
                        let Some(bytes) = message else { break };
                        if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                            closed.cancel();
                            break;
                        }
                    }
                }
            }
        });
        if let Some(stderr) = stderr {
            let tail = Arc::clone(&client.stderr_tail);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "pa_lsp::server", %line, "server stderr");
                    let mut tail = lock(&tail);
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            });
        }
        let reader = Reader {
            pending: Arc::clone(&client.pending),
            writer: client.writer.clone(),
            closed: client.closed.clone(),
            on_notification,
            on_request,
        };
        tokio::spawn(reader.run(stdout));
        Ok(client)
    }

    /// Whether the connection is gone (the server exited or a write failed).
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    /// The server's last stderr lines, joined.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        lock(&self.stderr_tail)
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Send a request and wait for its answer.
    ///
    /// # Errors
    ///
    /// The server's error answer, the timeout, or a closed connection.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        if self.is_closed() {
            return Err(RequestError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(id, sender);
        let mut message = json!({"jsonrpc": "2.0", "id": id, "method": method});
        message["params"] = params;
        if self.writer.send(frame(&message)).is_err() {
            lock(&self.pending).remove(&id);
            return Err(RequestError::Closed);
        }
        let outcome = tokio::select! {
            answer = receiver => match answer {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(RequestError::Rpc(error)),
                Err(_) => Err(RequestError::Closed),
            },
            () = self.closed.cancelled() => Err(RequestError::Closed),
            () = tokio::time::sleep(timeout) => Err(RequestError::Timeout(timeout)),
        };
        // Tell the server an answer is no longer wanted.
        if outcome.is_err() && lock(&self.pending).remove(&id).is_some() && !self.is_closed() {
            let _ = self.notify("$/cancelRequest", json!({"id": id}));
        }
        outcome
    }

    /// Send a notification.
    ///
    /// # Errors
    ///
    /// The connection is closed.
    pub fn notify(&self, method: &str, params: Value) -> Result<(), RequestError> {
        if self.is_closed() {
            return Err(RequestError::Closed);
        }
        let mut message = json!({"jsonrpc": "2.0", "method": method});
        message["params"] = params;
        self.writer
            .send(frame(&message))
            .map_err(|_| RequestError::Closed)
    }

    /// End the server: `shutdown` and `exit` within `timeout`, then a kill
    /// if it is still running.
    pub async fn shutdown(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        if !self.is_closed() {
            let _ = self.request("shutdown", Value::Null, timeout).await;
            let _ = self.notify("exit", Value::Null);
        }
        let mut child = self.child.lock().await;
        if let Some(process) = child.as_mut() {
            if tokio::time::timeout_at(deadline, process.wait())
                .await
                .is_err()
            {
                let _ = process.start_kill();
                let _ = process.wait().await;
            }
        }
        *child = None;
        self.closed.cancel();
    }
}

/// The reading half: parses frames and dispatches them.
struct Reader {
    pending: Pending,
    writer: mpsc::UnboundedSender<Vec<u8>>,
    closed: CancellationToken,
    on_notification: NotificationHandler,
    on_request: RequestHandler,
}

impl Reader {
    async fn run(self, stdout: tokio::process::ChildStdout) {
        let mut reader = BufReader::new(stdout);
        loop {
            let message = tokio::select! {
                () = self.closed.cancelled() => break,
                message = read_message(&mut reader) => message,
            };
            match message {
                Ok(Some(message)) => self.dispatch(&message),
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(target: "pa_lsp::server", %error, "unreadable message from the server; closing");
                    break;
                }
            }
        }
        self.closed.cancel();
        // Fail every waiting request.
        lock(&self.pending).clear();
    }

    fn dispatch(&self, message: &Value) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let answer = match (self.on_request)(method, params) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(error) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": error.code, "message": error.message},
                    }),
                };
                let _ = self.writer.send(frame(&answer));
            }
            (Some(method), None) => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                (self.on_notification)(method, params);
            }
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else {
                    return;
                };
                let Some(sender) = lock(&self.pending).remove(&id) else {
                    return;
                };
                let outcome = match message.get("error") {
                    Some(error) => Err(RpcError {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_string(),
                    }),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = sender.send(outcome);
            }
            (None, None) => {}
        }
    }
}

/// Read one framed message; `None` at a clean end of stream.
async fn read_message<R>(reader: &mut R) -> std::io::Result<Option<Value>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut length: Option<usize> = None;
    let mut saw_header = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return if saw_header {
                Err(std::io::ErrorKind::UnexpectedEof.into())
            } else {
                Ok(None)
            };
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if saw_header {
                break;
            }
            continue;
        }
        saw_header = true;
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok();
            }
        }
    }
    let length = length
        .filter(|length| *length <= MAX_MESSAGE_BYTES)
        .ok_or_else(|| std::io::Error::other("missing or oversized Content-Length"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| std::io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_with_extra_headers() {
        let first = frame(&json!({"a": 1}));
        let mut bytes = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n".to_vec();
        bytes.extend_from_slice(&first);
        bytes.extend_from_slice(&frame(&json!({"b": "\u{e9}"})));
        let mut reader = BufReader::new(bytes.as_slice());
        assert_eq!(
            read_message(&mut reader).await.unwrap(),
            Some(json!({"a": 1}))
        );
        assert_eq!(
            read_message(&mut reader).await.unwrap(),
            Some(json!({"b": "\u{e9}"}))
        );
        assert_eq!(read_message(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_truncated_frame_is_an_error() {
        let mut reader = BufReader::new(b"Content-Length: 10\r\n\r\n{}".as_slice());
        assert!(read_message(&mut reader).await.is_err());
    }
}
