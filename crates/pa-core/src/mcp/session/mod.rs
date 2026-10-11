//! Host-owned MCP client sessions: the connections behind the kernel's
//! `rlm.mcp.list_tools` / `call_tool` / `describe_tool` / `search_tools` /
//! `reload` / `close` (and `rlm.McpIntegration`), run with the official Rust
//! SDK (`rmcp`) over stdio child processes and streamable HTTP.
//!
//! Lifetime: one [`McpSessions`] per agent session, shared by every kernel
//! the session boots, so a kernel restart keeps its connections. A
//! connection ("generation") opens lazily on first use, is reused while the
//! server's resolved configuration (credential identity included) is
//! unchanged and the connection is alive, and is closed when that
//! configuration changes, on `rlm.mcp.reload` / `close`, when it sits idle
//! past the idle timeout, when the session ends (`dispose_kernel`), and when
//! the session drops (its server processes are killed with their trees).
//!
//! Wire: each `mcp.session.*` / `mcp.integration.*` host request answers
//! `{"ok": true, "value": …, "connected": bool}` or `{"ok": false,
//! "error": {"type": <Python exception class>, "message": …}, "connected":
//! bool}`; `connected` says whether a live connection existed when the
//! request began. A `host_cancel` for a request cancels it (an in-flight
//! `tools/call` is abandoned and the server sent `notifications/cancelled`).

mod connect;
mod diagnostic;
mod error;
mod generation;
mod http_client;
mod result;
mod stdio;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use connect::{KernelEnv, McpCredentials};
use error::{McpErrorKind, McpSessionError};
use generation::{Discovery, Generation, SharedGeneration, Target};
use pa_types::sync::MutexExt;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::kernel::shared::{
    HostRequestHandlers,
    HostRequestPayload,
    KernelEnvironment,
    host_handler,
    host_request_cancellation,
};

/// A connection unused this long is closed (it reopens on next use).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_mins(10);
/// Deadlines of an `rlm.McpIntegration` per-call connection (the in-kernel
/// client's HTTP read bound).
const INTEGRATION_TIMEOUT_MS: u64 = 300_000;

/// Resolves a server name to its configuration (the `mcp.config` view):
/// `{}` when the server is not declared.
pub(crate) type ConfigResolver = Box<dyn Fn(&str) -> anyhow::Result<Value> + Send + Sync>;

/// What a session's MCP connections inherit from the session.
#[derive(Debug, Clone, Default)]
pub struct McpSessionOptions {
    /// The session's working directory: a stdio server without its own
    /// `cwd` runs here (follow `/cwd` with [`McpSessions::set_cwd`]).
    pub cwd: PathBuf,
    /// The `kernel.environment` policy: stdio servers see the environment
    /// the kernel sees.
    pub environment: KernelEnvironment,
    /// Variables the host sets for the kernel process on top of what it
    /// inherits.
    pub kernel_env: HashMap<String, String>,
    /// Idle timeout ([`DEFAULT_IDLE_TIMEOUT`] when `None`).
    pub idle_timeout: Option<Duration>,
    /// The session's OS sandbox: stdio servers spawn under it, like the
    /// kernel whose `rlm.mcp` calls they serve. `None` spawns them as before.
    pub sandbox: Option<crate::os_sandbox::SessionSandbox>,
    /// The session's plan mode: while it is on (and OS-enforced) a stdio
    /// server starts under plan mode's `read-only` sandbox instead, and one
    /// started under another policy is restarted on its next use.
    pub plan_mode: Option<crate::kernel::plan_guard::PlanMode>,
}

/// The session's MCP connections. Clones share them.
#[derive(Clone)]
pub struct McpSessions {
    inner: Arc<Inner>,
}

struct Inner {
    configs: ConfigResolver,
    credentials: McpCredentials,
    env: KernelEnv,
    cwd: Mutex<PathBuf>,
    slots: Mutex<HashMap<String, SharedGeneration>>,
    idle_timeout: Duration,
    reaper_started: AtomicBool,
    sandbox: Option<crate::os_sandbox::SessionSandbox>,
    plan_mode: Option<crate::kernel::plan_guard::PlanMode>,
}

impl McpSessions {
    pub(crate) fn new(
        configs: ConfigResolver,
        auth: Arc<tokio::sync::Mutex<crate::auth::AuthStorage>>,
        usage: Option<super::McpUsageReporter>,
        options: McpSessionOptions,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                configs,
                credentials: McpCredentials::new(auth, usage),
                env: KernelEnv::new(options.environment, options.kernel_env),
                cwd: Mutex::new(options.cwd),
                slots: Mutex::new(HashMap::new()),
                idle_timeout: options.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT),
                reaper_started: AtomicBool::new(false),
                sandbox: options.sandbox,
                plan_mode: options.plan_mode,
            }),
        }
    }

    /// Follow the session's working directory (the next stdio server
    /// without its own `cwd` starts there).
    pub fn set_cwd(&self, cwd: &std::path::Path) {
        *self.inner.cwd.lock_or_recover() = cwd.to_path_buf();
    }

    /// Close every connection (each within its bounded shutdown); the
    /// session ended. A later use opens afresh.
    pub async fn close_all(&self) {
        self.inner.close(None).await;
    }

    /// The number of live connections (observability and tests).
    pub async fn open_connections(&self) -> usize {
        let slots: Vec<SharedGeneration> = self
            .inner
            .slots
            .lock_or_recover()
            .values()
            .cloned()
            .collect();
        let mut open = 0;
        for slot in slots {
            if slot.lock().await.is_some() {
                open += 1;
            }
        }
        open
    }

    /// Register the `mcp.session.*` and `mcp.integration.*` host requests.
    pub(crate) fn register_handlers(&self, handlers: &mut HostRequestHandlers) {
        type Op = fn(Arc<Inner>, Value, CancellationToken) -> OpFuture;
        let ops: [(&str, Op); 8] = [
            ("mcp.session.list_tools", |inner, data, cancel| {
                Box::pin(async move { inner.list_tools(&data, &cancel).await })
            }),
            ("mcp.session.call_tool", |inner, data, cancel| {
                Box::pin(async move { inner.call_tool(&data, &cancel).await })
            }),
            ("mcp.session.describe_tool", |inner, data, cancel| {
                Box::pin(async move { inner.describe_tool(&data, &cancel).await })
            }),
            ("mcp.session.search_tools", |inner, data, cancel| {
                Box::pin(async move { inner.search_tools(&data, &cancel).await })
            }),
            ("mcp.session.reload", |inner, data, _cancel| {
                Box::pin(async move { inner.reload(&data).await })
            }),
            ("mcp.session.close", |inner, _data, _cancel| {
                Box::pin(async move {
                    inner.close(None).await;
                    (false, Ok(Value::Null))
                })
            }),
            ("mcp.integration.list_tools", |inner, data, cancel| {
                Box::pin(async move { inner.integration(&data, Discovery::Full, &cancel).await })
            }),
            ("mcp.integration.call_tool", |inner, data, cancel| {
                Box::pin(async move { inner.integration(&data, Discovery::Skip, &cancel).await })
            }),
        ];
        for (request_type, op) in ops {
            let inner = Arc::clone(&self.inner);
            handlers.register(
                request_type,
                host_handler(move |payload: HostRequestPayload| {
                    let inner = Arc::clone(&inner);
                    async move {
                        let cancel = host_request_cancellation().unwrap_or_default();
                        let (connected, result) = op(inner, payload.data, cancel).await;
                        Ok(match result {
                            Ok(value) => json!({ "ok": true, "value": value, "connected": connected }),
                            Err(error) => {
                                json!({ "ok": false, "error": error.to_wire(), "connected": connected })
                            }
                        })
                    }
                }),
            );
        }
    }
}

type OpFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = (bool, Result<Value, McpSessionError>)> + Send>,
>;

fn required_str<'a>(data: &'a Value, key: &str) -> Result<&'a str, McpSessionError> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            McpSessionError::new(
                McpErrorKind::Value,
                format!("{key} must be a non-empty string"),
            )
        })
}

fn arguments(data: &Value) -> Result<Map<String, Value>, McpSessionError> {
    match data.get("arguments") {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(map)) => Ok(map.clone()),
        Some(_) => Err(McpSessionError::value("arguments must be a dict or None")),
    }
}

impl Inner {
    fn slot(&self, server: &str) -> SharedGeneration {
        Arc::clone(
            self.slots
                .lock_or_recover()
                .entry(server.to_string())
                .or_default(),
        )
    }

    /// The sandbox a stdio server started now runs under.
    fn stdio_sandbox(&self) -> Option<crate::os_sandbox::SessionSandbox> {
        match &self.plan_mode {
            Some(plan) => plan.spawn_sandbox(self.sandbox.as_ref()),
            None => self.sandbox.clone(),
        }
    }

    /// The server's configuration with its credential identity (and, for a
    /// stdio server, the sandbox mode it would start under), as the
    /// connection fingerprint.
    async fn resolve_config(&self, server: &str) -> Result<Value, McpSessionError> {
        let config = (self.configs)(server).map_err(|_| {
            McpSessionError::runtime(format!("Could not load MCP configuration for '{server}'"))
        })?;
        let Value::Object(mut map) = config else {
            return Err(McpSessionError::new(
                McpErrorKind::Key,
                format!("MCP server '{server}' is not declared in user settings"),
            ));
        };
        if map.is_empty() {
            return Err(McpSessionError::new(
                McpErrorKind::Key,
                format!("MCP server '{server}' is not declared in user settings"),
            ));
        }
        if map.get("enabled").and_then(Value::as_bool) == Some(false) {
            return Err(McpSessionError::runtime(format!(
                "MCP server '{server}' is disabled"
            )));
        }
        let config = Value::Object(map.clone());
        if config.get("type").and_then(Value::as_str) == Some("http")
            && config.get("credentialSource").and_then(Value::as_str) != Some("acp")
        {
            let identity =
                connect::auth_identity(server, &config, &self.env, &self.credentials).await?;
            map.insert("_authIdentity".to_string(), Value::String(identity));
        }
        if config.get("type").and_then(Value::as_str) == Some("stdio") {
            // A plan-mode toggle changes the policy a stdio server must run
            // under: the server started under the other one is retired.
            let mode = self
                .stdio_sandbox()
                .map_or(crate::os_sandbox::SandboxMode::Off, |sandbox| {
                    sandbox.mode()
                });
            map.insert(
                "_sandbox".to_string(),
                Value::String(mode.wire_name().to_string()),
            );
        }
        Ok(Value::Object(map))
    }

    async fn target(&self, server: &str, config: &Value) -> Result<Target, McpSessionError> {
        match config.get("type").and_then(Value::as_str) {
            Some("http") => Ok(Target::Http {
                url: connect::http_url(server, config)?,
                headers: connect::http_headers(server, config, &self.env, &self.credentials)
                    .await?,
            }),
            Some("stdio") => {
                let cwd = self.cwd.lock_or_recover().clone();
                Ok(Target::Stdio(
                    connect::stdio_launch(server, config, &self.env, &cwd)?,
                    self.stdio_sandbox().map(Box::new),
                ))
            }
            _ => Err(McpSessionError::value(format!(
                "MCP server '{server}' has unsupported transport {}",
                connect::transport_repr(config.get("type"))
            ))),
        }
    }

    /// Run `op` on the server's live connection, (re)opening it first when
    /// there is none, it died, or its configuration changed. Operations on
    /// one server run one at a time (an in-flight open is waited for).
    async fn with_generation<T>(
        self: &Arc<Self>,
        server: &str,
        cancel: &CancellationToken,
        op: impl AsyncFnOnce(&mut Generation) -> Result<T, McpSessionError>,
    ) -> (bool, Result<T, McpSessionError>) {
        let slot = self.slot(server);
        let mut guard = tokio::select! {
            guard = slot.lock() => guard,
            () = cancel.cancelled() => return (false, Err(McpSessionError::cancelled())),
        };
        let connected = guard.as_mut().is_some_and(Generation::is_alive);
        let config = match self.resolve_config(server).await {
            Ok(config) => config,
            // A failed resolution leaves the cached connection as it was.
            Err(error) => return (connected, Err(error)),
        };
        let reusable = guard
            .as_mut()
            .is_some_and(|generation| generation.config == config && generation.is_alive());
        if !reusable {
            if let Some(stale) = guard.take() {
                stale.close().await;
            }
            let opened = async {
                let target = self.target(server, &config).await?;
                Generation::open(server, config, target, Discovery::Full).await
            };
            let opened = tokio::select! {
                opened = opened => opened,
                () = cancel.cancelled() => Err(McpSessionError::cancelled()),
            };
            match opened {
                Ok(generation) => {
                    *guard = Some(generation);
                    self.ensure_reaper();
                }
                Err(error) => return (connected, Err(error)),
            }
        }
        let Some(generation) = guard.as_mut() else {
            return (
                connected,
                Err(McpSessionError::runtime("MCP connection unavailable")),
            );
        };
        generation.touch();
        let result = op(generation).await;
        generation.touch();
        (connected, result)
    }

    async fn list_tools(
        self: &Arc<Self>,
        data: &Value,
        cancel: &CancellationToken,
    ) -> (bool, Result<Value, McpSessionError>) {
        let server = match required_str(data, "server") {
            Ok(server) => server,
            Err(error) => return (false, Err(error)),
        };
        self.with_generation(server, cancel, async |generation| {
            Ok(Value::Array(
                generation
                    .allowed_tools()
                    .map(|(_, entry)| entry.clone())
                    .collect(),
            ))
        })
        .await
    }

    async fn describe_tool(
        self: &Arc<Self>,
        data: &Value,
        cancel: &CancellationToken,
    ) -> (bool, Result<Value, McpSessionError>) {
        let (server, tool) = match (required_str(data, "server"), required_str(data, "tool")) {
            (Ok(server), Ok(tool)) => (server, tool),
            (Err(error), _) | (_, Err(error)) => return (false, Err(error)),
        };
        self.with_generation(server, cancel, async |generation| {
            let Some(entry) = generation.tool(tool).cloned() else {
                return Err(McpSessionError::no_tool(server, tool));
            };
            if !generation.allows(tool) {
                return Err(McpSessionError::tool_disabled(server, tool));
            }
            Ok(entry)
        })
        .await
    }

    async fn search_tools(
        self: &Arc<Self>,
        data: &Value,
        cancel: &CancellationToken,
    ) -> (bool, Result<Value, McpSessionError>) {
        let (server, query) = match (required_str(data, "server"), required_str(data, "query")) {
            (Ok(server), Ok(query)) => (server, query.to_lowercase()),
            (Err(error), _) | (_, Err(error)) => return (false, Err(error)),
        };
        let limit = data
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(usize::MAX, |limit| limit as usize);
        self.with_generation(server, cancel, async |generation| {
            let matches = generation
                .allowed_tools()
                .filter(|(name, entry)| {
                    let description = entry
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    format!("{name}\n{description}")
                        .to_lowercase()
                        .contains(&query)
                })
                .take(limit)
                .map(|(name, entry)| {
                    json!({
                        "connectionId": server,
                        "name": name,
                        "description": entry.get("description").cloned().unwrap_or_else(|| json!("")),
                    })
                })
                .collect();
            Ok(Value::Array(matches))
        })
        .await
    }

    async fn call_tool(
        self: &Arc<Self>,
        data: &Value,
        cancel: &CancellationToken,
    ) -> (bool, Result<Value, McpSessionError>) {
        let (server, tool, arguments) = match (
            required_str(data, "server"),
            required_str(data, "tool"),
            arguments(data),
        ) {
            (Ok(server), Ok(tool), Ok(arguments)) => (server, tool, arguments),
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                return (false, Err(error));
            }
        };
        self.with_generation(server, cancel, async move |generation| {
            generation.call(tool, arguments, cancel).await
        })
        .await
    }

    async fn reload(&self, data: &Value) -> (bool, Result<Value, McpSessionError>) {
        let server = match data.get("server") {
            None | Some(Value::Null) => None,
            Some(Value::String(server)) if !server.is_empty() => Some(server.as_str()),
            Some(_) => {
                return (
                    false,
                    Err(McpSessionError::value("server must be a non-empty string")),
                );
            }
        };
        self.close(server).await;
        (false, Ok(Value::Null))
    }

    /// Close one server's connection (or every connection), waiting for
    /// in-flight work on it; closes run concurrently.
    async fn close(&self, server: Option<&str>) {
        let slots: Vec<SharedGeneration> = {
            let slots = self.slots.lock_or_recover();
            match server {
                Some(server) => slots.get(server).cloned().into_iter().collect(),
                None => slots.values().cloned().collect(),
            }
        };
        futures::future::join_all(slots.into_iter().map(|slot| async move {
            let generation = slot.lock().await.take();
            if let Some(generation) = generation {
                generation.close().await;
            }
        }))
        .await;
    }

    /// An `rlm.McpIntegration` request: a per-call HTTP connection to the
    /// integration's own endpoint with the headers it resolved.
    async fn integration(
        &self,
        data: &Value,
        discovery: Discovery,
        cancel: &CancellationToken,
    ) -> (bool, Result<Value, McpSessionError>) {
        let run = async {
            let server = required_str(data, "server")?;
            let url = required_str(data, "url")?;
            let headers = match data.get("headers") {
                None => Map::new(),
                Some(Value::Object(headers)) => headers.clone(),
                Some(_) => {
                    return Err(McpSessionError::value(
                        "MCP HTTP headers must contain strings",
                    ));
                }
            };
            let config = json!({
                "type": "http",
                "url": url,
                "headers": headers,
                "startupTimeoutMs": INTEGRATION_TIMEOUT_MS,
                "callTimeoutMs": INTEGRATION_TIMEOUT_MS,
            });
            let target = Target::Http {
                url: url.to_string(),
                headers: connect::string_headers(&config)?,
            };
            let generation = Generation::open(server, config, target, discovery).await?;
            let result = match discovery {
                Discovery::Full => Ok(Value::Array(
                    generation
                        .allowed_tools()
                        .map(|(_, entry)| entry.clone())
                        .collect(),
                )),
                Discovery::Skip => {
                    let tool = required_str(data, "tool");
                    match (tool, arguments(data)) {
                        (Ok(tool), Ok(arguments)) => {
                            generation.call_unchecked(tool, arguments, cancel).await
                        }
                        (Err(error), _) | (_, Err(error)) => Err(error),
                    }
                }
            };
            generation.close().await;
            result
        };
        let result = tokio::select! {
            result = run => result,
            () = cancel.cancelled() => Err(McpSessionError::cancelled()),
        };
        (false, result)
    }

    fn ensure_reaper(self: &Arc<Self>) {
        if self.reaper_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak: Weak<Self> = Arc::downgrade(self);
        let period =
            (self.idle_timeout / 4).clamp(Duration::from_millis(10), Duration::from_secs(60));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                inner.reap_idle().await;
            }
        });
    }

    /// Close every connection idle past the timeout (or already dead); a
    /// connection in use is skipped.
    async fn reap_idle(&self) {
        let slots: Vec<SharedGeneration> = self.slots.lock_or_recover().values().cloned().collect();
        for slot in slots {
            let Ok(mut guard) = slot.try_lock() else {
                continue;
            };
            let expired = guard.as_mut().is_some_and(|generation| {
                generation.idle_for() >= self.idle_timeout || !generation.is_alive()
            });
            if expired {
                let generation = guard.take();
                drop(guard);
                if let Some(generation) = generation {
                    tracing::debug!(target: "pa_core::mcp", server = %generation.server, "closing idle MCP connection");
                    generation.close().await;
                }
            }
        }
    }
}

// The fixtures spawn POSIX process trees.
#[cfg(all(test, unix))]
mod tests;
