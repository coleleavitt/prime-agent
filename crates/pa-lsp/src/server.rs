//! One configured language server: started on first use, initialized the
//! way Claude Code initializes it, restarted after a crash a bounded number
//! of times, and shut down with its session.
//!
//! Crash recovery differs from Claude Code in one way: Claude Code resets
//! its crash count after every successful start, so a server that starts
//! and then crashes on every request restarts forever. Here every crash and
//! every failed start counts for the server's whole session, so recovery is
//! bounded by `maxRestarts` (default 3); `restartOnCrash: false` gives up at
//! the first crash.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::ServerConfig;
use crate::protocol::{CONTENT_MODIFIED, METHOD_NOT_FOUND, path_to_uri};
use crate::rpc::{
    NotificationHandler,
    RequestError,
    RequestHandler,
    RpcClient,
    RpcError,
    SpawnSpec,
};

/// Claude Code's `ContentModified` retries (500 ms, 1 s, 2 s).
const CONTENT_MODIFIED_RETRIES: u32 = 3;
const CONTENT_MODIFIED_BACKOFF: Duration = Duration::from_millis(500);
/// How long a request that reads the server's index waits for its work-done
/// progress (clangd loading or building its background index) to end.
const INDEX_WAIT: Duration = Duration::from_secs(20);
/// A server that just started reports its first progress shortly after
/// `initialized`; no progress before this is not yet "ready".
const INDEX_SETTLE: Duration = Duration::from_secs(1);

/// What a server tells its owner.
pub trait ServerEvents: Send + Sync {
    /// A notification (`textDocument/publishDiagnostics`, log messages, ...).
    fn notification(&self, server: &str, method: &str, params: Value);
    /// The server (re)started: every document it had open is gone.
    fn started(&self, server: &str);
}

/// A server's state, for status reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStatus {
    /// Not started yet (or stopped by its session).
    Stopped,
    Running,
    /// It crashed or failed to start; the next use restarts it.
    Error,
    /// It crashed or failed more often than its configuration allows.
    Failed,
}

impl ServerStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::Error => "error",
            Self::Failed => "failed",
        }
    }
}

#[derive(Default)]
struct State {
    client: Option<Arc<RpcClient>>,
    /// Crashes and failed starts, for the session's lifetime.
    failures: u32,
    starts: u32,
    last_error: Option<String>,
    /// The session ended: never start again.
    ended: bool,
}

/// One server of one session.
pub struct LspServer {
    name: String,
    config: ServerConfig,
    root: PathBuf,
    state: tokio::sync::Mutex<State>,
    events: Arc<dyn ServerEvents>,
    progress: Arc<Progress>,
}

/// The work-done progress a running server has begun and not ended, by
/// token, and when the server started.
#[derive(Default)]
struct Progress {
    /// Token -> (title, latest message).
    active: std::sync::Mutex<std::collections::BTreeMap<String, (String, String)>>,
    started: std::sync::Mutex<Option<tokio::time::Instant>>,
}

impl Progress {
    fn restart(&self) {
        lock(&self.active).clear();
        *lock(&self.started) = Some(tokio::time::Instant::now());
    }

    /// Apply a `$/progress` notification.
    fn update(&self, params: &Value) {
        let token = match &params["token"] {
            Value::String(token) => token.clone(),
            other => other.to_string(),
        };
        let value = &params["value"];
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let mut active = lock(&self.active);
        match value.get("kind").and_then(Value::as_str) {
            Some("begin") => {
                active.insert(
                    token,
                    (
                        text("title").unwrap_or_default(),
                        text("message").unwrap_or_default(),
                    ),
                );
            }
            Some("report") => {
                if let (Some(entry), Some(message)) = (active.get_mut(&token), text("message")) {
                    entry.1 = message;
                }
            }
            Some("end") => {
                active.remove(&token);
            }
            _ => {}
        }
    }

    /// What is still in progress, or `None` once the server settled with
    /// nothing in progress.
    fn pending(&self) -> Option<String> {
        let active = lock(&self.active);
        if !active.is_empty() {
            let shown: Vec<String> = active
                .values()
                .map(|(title, message)| format!("{title} {message}").trim().to_string())
                .collect();
            return Some(shown.join("; "));
        }
        let started = *lock(&self.started);
        started
            .is_some_and(|started| started.elapsed() < INDEX_SETTLE)
            .then(String::new)
    }
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl std::fmt::Debug for LspServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspServer")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// The section of `settings` a `workspace/configuration` item asks for
/// (a dotted path; the whole value when empty); null when absent.
fn settings_section(settings: Option<&Value>, section: Option<&str>) -> Value {
    let Some(mut value) = settings else {
        return Value::Null;
    };
    let Some(section) = section.filter(|section| !section.is_empty()) else {
        return value.clone();
    };
    for part in section.split('.') {
        match value.get(part) {
            Some(child) => value = child,
            None => return Value::Null,
        }
    }
    value.clone()
}

impl LspServer {
    #[must_use]
    pub fn new(
        name: String,
        config: ServerConfig,
        cwd: &Path,
        events: Arc<dyn ServerEvents>,
    ) -> Self {
        let root = config.root(cwd);
        Self {
            name,
            config,
            root,
            state: tokio::sync::Mutex::new(State::default()),
            events,
            progress: Arc::default(),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// The status, the starts so far, and the last error.
    pub async fn status(&self) -> (ServerStatus, u32, Option<String>) {
        let state = self.state.lock().await;
        let status = match &state.client {
            Some(client) if !client.is_closed() => ServerStatus::Running,
            Some(_) => ServerStatus::Error,
            None if self.gave_up(&state).is_some() => ServerStatus::Failed,
            None if state.last_error.is_some() => ServerStatus::Error,
            None => ServerStatus::Stopped,
        };
        (status, state.starts, state.last_error.clone())
    }

    /// Crashes and failed starts so far.
    pub async fn failures(&self) -> u32 {
        self.state.lock().await.failures
    }

    /// Whether the server is running now (without starting it).
    pub async fn is_running(&self) -> bool {
        self.state
            .lock()
            .await
            .client
            .as_ref()
            .is_some_and(|client| !client.is_closed())
    }

    fn gave_up(&self, state: &State) -> Option<String> {
        if state.failures == 0 {
            return None;
        }
        let last = state
            .last_error
            .as_deref()
            .map(|error| format!("; last error: {error}"))
            .unwrap_or_default();
        if self.config.restart_on_crash == Some(false) {
            return Some(format!(
                "LSP server '{}' crashed (restartOnCrash is false){last}",
                self.name
            ));
        }
        let max = self.config.max_restarts();
        (state.failures > max).then(|| {
            format!(
                "LSP server '{}' exceeded max crash recovery attempts ({max}){last}",
                self.name
            )
        })
    }

    /// The running server's connection, starting it when needed.
    ///
    /// # Errors
    ///
    /// The session ended, the server gave up, or it failed to start.
    pub async fn ensure_running(&self) -> Result<Arc<RpcClient>, String> {
        let mut state = self.state.lock().await;
        if state.ended {
            return Err(format!("LSP server '{}' was shut down", self.name));
        }
        if let Some(client) = &state.client {
            if !client.is_closed() {
                return Ok(Arc::clone(client));
            }
            let tail = client.stderr_tail();
            state.client = None;
            state.failures += 1;
            state.last_error = Some(if tail.is_empty() {
                format!("LSP server '{}' exited", self.name)
            } else {
                format!("LSP server '{}' exited: {tail}", self.name)
            });
            tracing::warn!(target: "pa_lsp::server", server = %self.name, "the language server exited");
        }
        if let Some(reason) = self.gave_up(&state) {
            return Err(reason);
        }
        match self.start().await {
            Ok(client) => {
                state.starts += 1;
                state.client = Some(Arc::clone(&client));
                drop(state);
                self.events.started(&self.name);
                Ok(client)
            }
            Err(error) => {
                state.failures += 1;
                state.last_error = Some(error.clone());
                Err(error)
            }
        }
    }

    fn handlers(&self) -> (NotificationHandler, RequestHandler) {
        let events = Arc::clone(&self.events);
        let progress = Arc::clone(&self.progress);
        let name = self.name.clone();
        let on_notification: NotificationHandler = Arc::new(move |method, params| {
            if method == "$/progress" {
                progress.update(&params);
            }
            events.notification(&name, method, params);
        });
        let settings = self.config.settings.clone();
        let name = self.name.clone();
        let on_request: RequestHandler = Arc::new(move |method, params| match method {
            "workspace/configuration" => Ok(Value::Array(
                params
                    .get("items")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|item| {
                        settings_section(
                            settings.as_ref(),
                            item.get("section").and_then(Value::as_str),
                        )
                    })
                    .collect(),
            )),
            // Claude Code ignores dynamic registration; progress tokens and
            // message requests need no answer beyond null.
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "window/showMessageRequest"
            | "window/showDocument" => Ok(Value::Null),
            "workspace/applyEdit" => {
                Ok(json!({"applied": false, "failureReason": "the client does not apply edits"}))
            }
            other => {
                tracing::debug!(target: "pa_lsp::server", server = %name, method = %other, "unhandled server request");
                Err(RpcError {
                    code: METHOD_NOT_FOUND,
                    message: format!("unhandled method {other}"),
                })
            }
        });
        (on_notification, on_request)
    }

    fn initialize_params(&self) -> Value {
        let root_uri = path_to_uri(&self.root);
        let name = self
            .root
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "prime-agent", "version": env!("CARGO_PKG_VERSION")},
            "initializationOptions": self.config.initialization_options.clone().unwrap_or_else(|| json!({})),
            "workspaceFolders": [{"uri": root_uri, "name": name}],
            "rootPath": self.root.display().to_string(),
            "rootUri": root_uri,
            "capabilities": {
                "workspace": {
                    "configuration": self.config.settings.is_some(),
                    "workspaceFolders": false,
                    "didChangeWatchedFiles": {"dynamicRegistration": false},
                },
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "willSave": false,
                        "willSaveWaitUntil": false,
                        "didSave": true,
                    },
                    "publishDiagnostics": {
                        "relatedInformation": true,
                        "tagSupport": {"valueSet": [1, 2]},
                        "versionSupport": false,
                        "codeDescriptionSupport": true,
                        "dataSupport": false,
                    },
                    "hover": {"dynamicRegistration": false, "contentFormat": ["markdown", "plaintext"]},
                    "definition": {"dynamicRegistration": false, "linkSupport": true},
                    "implementation": {"dynamicRegistration": false, "linkSupport": true},
                    "references": {"dynamicRegistration": false},
                    "documentSymbol": {"dynamicRegistration": false, "hierarchicalDocumentSymbolSupport": true},
                    "callHierarchy": {"dynamicRegistration": false},
                },
                "general": {"positionEncodings": ["utf-16"]},
            },
        })
    }

    async fn start(&self) -> Result<Arc<RpcClient>, String> {
        let spec = SpawnSpec {
            command: self.config.command.clone(),
            args: self.config.args.clone(),
            env: self
                .config
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            cwd: self.root.clone(),
        };
        let (on_notification, on_request) = self.handlers();
        self.progress.restart();
        tracing::debug!(target: "pa_lsp::server", server = %self.name, "starting the language server");
        let client = RpcClient::spawn(&spec, on_notification, on_request).map_err(|error| {
            format!(
                "Failed to start LSP server '{}' ({}): {error}",
                self.name, self.config.command
            )
        })?;
        let timeout = self.config.startup_timeout();
        let initialized = client
            .request("initialize", self.initialize_params(), timeout)
            .await;
        if let Err(error) = initialized {
            let tail = client.stderr_tail();
            client.shutdown(self.config.shutdown_timeout()).await;
            let detail = if tail.is_empty() {
                String::new()
            } else {
                format!("; stderr: {tail}")
            };
            return Err(match error {
                RequestError::Timeout(_) => format!(
                    "LSP server '{}' timed out after {}ms during initialization{detail}",
                    self.name,
                    timeout.as_millis()
                ),
                RequestError::Rpc(_) | RequestError::Closed => format!(
                    "Failed to start LSP server '{}': {error}{detail}",
                    self.name
                ),
            });
        }
        let _ = client.notify("initialized", json!({}));
        if let Some(settings) = &self.config.settings {
            let _ = client.notify(
                "workspace/didChangeConfiguration",
                json!({"settings": settings}),
            );
        }
        Ok(client)
    }

    /// Send a request (starting the server when needed), retrying a
    /// `ContentModified` answer like Claude Code.
    ///
    /// # Errors
    ///
    /// The server could not run, or the request failed.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let client = self.ensure_running().await?;
        let timeout = self.config.request_timeout();
        let mut attempt = 0;
        loop {
            match client.request(method, params.clone(), timeout).await {
                Ok(value) => return Ok(value),
                Err(RequestError::Rpc(error))
                    if error.code == CONTENT_MODIFIED && attempt < CONTENT_MODIFIED_RETRIES =>
                {
                    tokio::time::sleep(CONTENT_MODIFIED_BACKOFF * 2u32.pow(attempt)).await;
                    attempt += 1;
                }
                Err(error) => {
                    return Err(format!(
                        "LSP request '{method}' failed for server '{}': {error}",
                        self.name
                    ));
                }
            }
        }
    }

    /// Wait (at most [`INDEX_WAIT`]) for the server's work-done progress to
    /// end, so an answer read from its index (references, calls, symbols)
    /// is not the partial one of a server still loading or building it.
    /// Returns what is still in progress when the wait ran out.
    pub async fn index_ready(&self) -> Option<String> {
        let deadline = tokio::time::Instant::now() + INDEX_WAIT;
        loop {
            let pending = self.progress.pending()?;
            if tokio::time::Instant::now() >= deadline {
                return Some(pending);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Send a notification on the running connection.
    ///
    /// # Errors
    ///
    /// The server is not running.
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let client = self.ensure_running().await?;
        client.notify(method, params).map_err(|error| {
            format!(
                "LSP notification '{method}' failed for server '{}': {error}",
                self.name
            )
        })
    }

    /// End the server for good (its session ended).
    pub async fn shutdown(&self) {
        let client = {
            let mut state = self.state.lock().await;
            state.ended = true;
            state.client.take()
        };
        if let Some(client) = client {
            client.shutdown(self.config.shutdown_timeout()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_is_pending_from_begin_to_end_and_just_after_a_start() {
        let progress = Progress::default();
        assert_eq!(progress.pending(), None);
        progress.restart();
        assert_eq!(progress.pending().as_deref(), Some(""));
        let started = tokio::time::Instant::now()
            .checked_sub(INDEX_SETTLE)
            .unwrap();
        *lock(&progress.started) = Some(started);
        assert_eq!(progress.pending(), None);

        let token = "backgroundIndexProgress";
        progress.update(&json!({"token": token, "value": {"kind": "begin", "title": "indexing"}}));
        progress
            .update(&json!({"token": token, "value": {"kind": "report", "message": "663/5652"}}));
        progress.update(&json!({"token": 7, "value": {"kind": "begin", "title": "loading"}}));
        assert_eq!(
            progress.pending().as_deref(),
            Some("loading; indexing 663/5652")
        );
        progress.update(&json!({"token": 7, "value": {"kind": "end"}}));
        progress.update(&json!({"token": token, "value": {"kind": "end"}}));
        assert_eq!(progress.pending(), None);
    }

    #[test]
    fn configuration_sections_follow_dotted_paths() {
        let settings = json!({"python": {"analysis": {"strict": true}}});
        assert_eq!(
            settings_section(Some(&settings), Some("python.analysis")),
            json!({"strict": true})
        );
        assert_eq!(settings_section(Some(&settings), Some("rust")), Value::Null);
        assert_eq!(settings_section(Some(&settings), None), settings);
        assert_eq!(settings_section(None, Some("python")), Value::Null);
    }
}
