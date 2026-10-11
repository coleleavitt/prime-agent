//! One live MCP connection ("generation"): opened with the startup handshake
//! and a complete tool inventory under the startup deadline, called under
//! the per-call deadline (and the caller's cancellation, which reaches the
//! server as `notifications/cancelled`), and closed within a bounded time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequest,
    CallToolRequestParams,
    CancelledNotificationParam,
    ClientConfig,
    ClientRequest,
    Implementation,
    ListToolsResult,
    PaginatedRequestParams,
    ProtocolVersion,
};
use rmcp::service::{PeerRequestOptions, RunningService, ServiceError};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt as _};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::connect::{self, StdioLaunch};
use super::diagnostic::{ORIGINAL_ERROR_BYTE_LIMIT, sanitize_diagnostic};
use super::error::{McpErrorKind, McpSessionError};
use super::http_client::McpHttpClient;
use super::result::{parse_call_result, tool_entry};
use super::stdio::StdioChild;

/// `tools/list` pages a server may take: a server that keeps paginating
/// must not wedge discovery.
pub(crate) const MAX_TOOL_PAGES: usize = 25;
/// Bound on closing one connection (the in-kernel client's own bound, kept
/// below the host's 5 s kernel shutdown deadline).
pub(crate) const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(2_500);
/// How long a failed stdio startup waits for the rest of the child's stderr.
const STDERR_SETTLE: Duration = Duration::from_millis(500);
/// Bound on telling a server a call was cancelled.
const CANCEL_NOTICE_TIMEOUT: Duration = Duration::from_secs(1);

type ClientService = RunningService<RoleClient, ClientConfig>;

/// Where a generation connects.
pub(crate) enum Target {
    /// A stdio server, spawned under the session's OS sandbox when it has
    /// one (boxed: the sandbox dwarfs the HTTP arm).
    Stdio(StdioLaunch, Option<Box<crate::os_sandbox::SessionSandbox>>),
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

/// Whether opening also fetches the complete tool inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Discovery {
    Full,
    Skip,
}

pub(crate) struct Generation {
    pub(crate) server: String,
    /// The resolved configuration (with its credential identity): a
    /// different one retires this generation.
    pub(crate) config: Value,
    /// The inventory in server order, one entry per name.
    tools: Vec<(String, Value)>,
    call_timeout: Duration,
    service: ClientService,
    child: Option<StdioChild>,
    last_used: Instant,
}

fn client_info() -> ClientConfig {
    let mut info = ClientConfig::default();
    // The `initialize` handshake (the newest version that still has one).
    info.protocol_version = ProtocolVersion::LATEST_WITH_INITIALIZE;
    info.client_info = Implementation::new("prime-agent", env!("CARGO_PKG_VERSION"));
    info
}

/// Why a startup handshake failed, before it is reported.
enum StartupFailure {
    Init(Box<rmcp::service::ClientInitializeError>),
    Inventory(McpSessionError),
}

impl StartupFailure {
    /// The in-kernel client's `f"{type(exc).__name__}: {exc}"` for the
    /// failure (the SDK's protocol errors were `MCPError`).
    fn describe(&self) -> String {
        match self {
            // A stdio server that went away mid-handshake, whichever side
            // noticed first.
            StartupFailure::Init(error) => match error.as_ref() {
                rmcp::service::ClientInitializeError::ConnectionClosed(_)
                | rmcp::service::ClientInitializeError::TransportError { .. } => {
                    "MCPError: Connection closed".to_string()
                }
                other => format!("MCPError: {}", init_error_text(other)),
            },
            StartupFailure::Inventory(error) => {
                format!("{}: {}", error.python_name(), error.message)
            }
        }
    }

    fn into_error(self, server: &str, redact: &[String]) -> McpSessionError {
        match self {
            StartupFailure::Init(error) => McpSessionError::runtime(sanitize_diagnostic(
                &format!(
                    "MCP server '{server}' failed to start: {}",
                    init_error_text(&error)
                ),
                redact,
                &[],
                ORIGINAL_ERROR_BYTE_LIMIT,
            )),
            StartupFailure::Inventory(error) => error,
        }
    }
}

fn init_error_text(error: &rmcp::service::ClientInitializeError) -> String {
    use rmcp::service::ClientInitializeError;
    match error {
        ClientInitializeError::ConnectionClosed(_) => "Connection closed".to_string(),
        ClientInitializeError::JsonRpcError(data) => data.message.to_string(),
        other => other.to_string(),
    }
}

/// A failed request as the caller sees it.
fn request_error(server: &str, error: ServiceError) -> McpSessionError {
    match error {
        ServiceError::Timeout { .. } => McpSessionError::timeout(),
        ServiceError::McpError(data) => McpSessionError::runtime(data.message.to_string()),
        ServiceError::TransportClosed => {
            McpSessionError::runtime(format!("MCP server '{server}' connection closed"))
        }
        other => McpSessionError::runtime(format!("MCP server '{server}': {other}")),
    }
}

impl Generation {
    /// Open a connection and, with [`Discovery::Full`], its tool inventory,
    /// within the configured startup deadline.
    ///
    /// # Errors
    ///
    /// `TimeoutError` past the deadline; for a stdio server whose handshake
    /// or inventory failed, `McpStartupError` with its redacted stderr tail;
    /// otherwise the failure itself.
    #[tracing::instrument(level = "debug", name = "mcp.session.open", skip_all, fields(mcp.server = %server))]
    pub(crate) async fn open(
        server: &str,
        config: Value,
        target: Target,
        discovery: Discovery,
    ) -> Result<Self, McpSessionError> {
        let (startup_timeout, call_timeout) = connect::timeouts(&config)?;
        match target {
            Target::Stdio(launch, sandbox) => {
                let (child, stdout, stdin) = StdioChild::spawn(&launch, sandbox.as_deref())?;
                let started = tokio::time::timeout(
                    startup_timeout,
                    handshake(server, (stdout, stdin), discovery),
                )
                .await;
                match started {
                    Ok(Ok((service, tools))) => {
                        child.stderr.stop_capture();
                        Ok(Self {
                            server: server.to_string(),
                            config,
                            tools,
                            call_timeout,
                            service,
                            child: Some(child),
                            last_used: Instant::now(),
                        })
                    }
                    Ok(Err(failure)) => {
                        let stderr = child.stderr.clone();
                        bounded(child.shutdown()).await;
                        stderr.wait_eof(STDERR_SETTLE).await;
                        Err(startup_error(&failure, &launch, &stderr))
                    }
                    Err(_elapsed) => {
                        bounded(child.shutdown()).await;
                        Err(McpSessionError::timeout())
                    }
                }
            }
            Target::Http { url, headers } => {
                let client = McpHttpClient::new(&headers, Some(call_timeout))?;
                let transport = StreamableHttpClientTransport::with_client(
                    client,
                    StreamableHttpClientTransportConfig::with_uri(url),
                );
                let redact: Vec<String> = headers.into_iter().map(|(_, value)| value).collect();
                match tokio::time::timeout(startup_timeout, handshake(server, transport, discovery))
                    .await
                {
                    Ok(Ok((service, tools))) => Ok(Self {
                        server: server.to_string(),
                        config,
                        tools,
                        call_timeout,
                        service,
                        child: None,
                        last_used: Instant::now(),
                    }),
                    Ok(Err(failure)) => Err(failure.into_error(server, &redact)),
                    Err(_elapsed) => Err(McpSessionError::timeout()),
                }
            }
        }
    }

    /// True while the connection is usable (its transport and, for stdio,
    /// its server process are alive).
    pub(crate) fn is_alive(&mut self) -> bool {
        !self.service.is_closed() && !self.child.as_mut().is_some_and(StdioChild::has_exited)
    }

    pub(crate) fn touch(&mut self) {
        self.last_used = Instant::now();
    }

    pub(crate) fn idle_for(&self) -> Duration {
        self.last_used.elapsed()
    }

    /// Whether the connection's `enabledTools` / `disabledTools` policy
    /// admits `tool`.
    pub(crate) fn allows(&self, tool: &str) -> bool {
        let listed = |key: &str| {
            self.config
                .get(key)
                .and_then(Value::as_array)
                .map(|names| names.iter().any(|name| name.as_str() == Some(tool)))
        };
        if listed("enabledTools") == Some(false) {
            return false;
        }
        listed("disabledTools") != Some(true)
    }

    /// The inventory entry for `tool`, if the server lists it.
    pub(crate) fn tool(&self, tool: &str) -> Option<&Value> {
        self.tools
            .iter()
            .find(|(name, _)| name == tool)
            .map(|(_, entry)| entry)
    }

    /// The policy-admitted inventory, in server order.
    pub(crate) fn allowed_tools(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.tools
            .iter()
            .filter(|(name, _)| self.allows(name))
            .map(|(name, entry)| (name.as_str(), entry))
    }

    /// Call `tool` under the per-call deadline; `cancel` abandons it (the
    /// server is told).
    ///
    /// # Errors
    ///
    /// `PermissionError` / `KeyError` for an excluded or unknown tool,
    /// `TimeoutError`, `CancelledError`, `McpToolError` for a result the
    /// server flagged, `RuntimeError` for a protocol or transport failure.
    #[tracing::instrument(level = "debug", name = "mcp.session.call", skip_all, fields(mcp.server = %self.server, mcp.tool = %tool))]
    pub(crate) async fn call(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
        cancel: &CancellationToken,
    ) -> Result<Value, McpSessionError> {
        if !self.allows(tool) {
            return Err(McpSessionError::tool_disabled(&self.server, tool));
        }
        if self.tool(tool).is_none() {
            return Err(McpSessionError::no_tool(&self.server, tool));
        }
        self.call_unchecked(tool, arguments, cancel).await
    }

    /// Call `tool` without consulting the inventory (an integration's
    /// per-call connection discovers nothing).
    pub(crate) async fn call_unchecked(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
        cancel: &CancellationToken,
    ) -> Result<Value, McpSessionError> {
        let peer = self.service.peer();
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            CallToolRequestParams::new(tool.to_string()).with_arguments(arguments),
        ));
        let handle = peer
            .send_cancellable_request(request, PeerRequestOptions::with_timeout(self.call_timeout))
            .await
            .map_err(|error| request_error(&self.server, error))?;
        let id = handle.id.clone();
        tokio::select! {
            response = handle.await_response() => {
                let response = response.map_err(|error| request_error(&self.server, error))?;
                // The union is untagged: read the result as sent, whichever
                // variant it decoded into.
                let result = serde_json::to_value(&response).map_err(|error| {
                    McpSessionError::runtime(format!("MCP server '{}': {error}", self.server))
                })?;
                parse_call_result(&result)
            }
            () = cancel.cancelled() => {
                let notice = peer.notify_cancelled(CancelledNotificationParam::new(
                    Some(id),
                    Some("cancelled by the caller".to_string()),
                ));
                let _ = tokio::time::timeout(CANCEL_NOTICE_TIMEOUT, notice).await;
                Err(McpSessionError::cancelled())
            }
        }
    }

    /// Close the connection: the transport first, then the server process
    /// (gracefully, then its whole tree), all within [`SHUTDOWN_TIMEOUT`].
    pub(crate) async fn close(self) {
        let Self { service, child, .. } = self;
        bounded(async move {
            let _ = service.cancel().await;
            if let Some(child) = child {
                child.shutdown().await;
            }
        })
        .await;
    }
}

/// Run `future` within [`SHUTDOWN_TIMEOUT`]; past it, whatever it owns is
/// dropped (a dropped server process is killed with its tree).
async fn bounded(future: impl std::future::Future<Output = ()>) {
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, future).await;
}

async fn handshake<T, E, A>(
    server: &str,
    transport: T,
    discovery: Discovery,
) -> Result<(ClientService, Vec<(String, Value)>), StartupFailure>
where
    T: rmcp::transport::IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let service = client_info()
        .serve(transport)
        .await
        .map_err(|error| StartupFailure::Init(Box::new(error)))?;
    let tools = match discovery {
        Discovery::Full => {
            let peer = service.peer().clone();
            discover(server, MAX_TOOL_PAGES, |cursor| {
                let peer = peer.clone();
                async move {
                    peer.list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                        .await
                }
            })
            .await
            .map_err(StartupFailure::Inventory)?
        }
        Discovery::Skip => Vec::new(),
    };
    Ok((service, tools))
}

/// Fetch the complete tool inventory, following `tools/list` cursors.
///
/// # Errors
///
/// `McpDiscoveryError` when pagination cannot complete honestly (a repeated
/// or empty cursor, or more than `max_pages` pages): a partial inventory is
/// never published as if complete.
pub(crate) async fn discover<F, Fut>(
    server: &str,
    max_pages: usize,
    mut page: F,
) -> Result<Vec<(String, Value)>, McpSessionError>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<ListToolsResult, ServiceError>>,
{
    let mut tools: Vec<(String, Value)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let response = page(cursor.take())
            .await
            .map_err(|error| request_error(server, error))?;
        pages += 1;
        for tool in &response.tools {
            let entry = tool_entry(tool);
            match tools
                .iter_mut()
                .find(|(name, _)| name == tool.name.as_ref())
            {
                Some(existing) => existing.1 = entry,
                None => tools.push((tool.name.to_string(), entry)),
            }
        }
        let Some(next) = response.next_cursor else {
            break;
        };
        if next.is_empty() {
            return Err(McpSessionError::new(
                McpErrorKind::Discovery,
                format!("MCP server '{server}' returned a malformed tools/list pagination cursor"),
            ));
        }
        if seen.contains(&next) {
            return Err(McpSessionError::new(
                McpErrorKind::Discovery,
                format!(
                    "MCP server '{server}' repeated a tools/list pagination cursor; \
                     its tool inventory cannot be completed"
                ),
            ));
        }
        if pages >= max_pages {
            return Err(McpSessionError::new(
                McpErrorKind::Discovery,
                format!(
                    "MCP server '{server}' paginated tools/list beyond {max_pages} pages; \
                     refusing to publish a partial tool inventory"
                ),
            ));
        }
        seen.insert(next.clone());
        cursor = Some(next);
    }
    Ok(tools)
}

/// `McpStartupError` for a stdio server whose handshake or inventory
/// failed: the redacted failure and stderr tail, or no child output at all
/// when a configured value is too short to redact safely.
fn startup_error(
    failure: &StartupFailure,
    launch: &StdioLaunch,
    stderr: &super::diagnostic::StderrTail,
) -> McpSessionError {
    let (original, tail) = if launch.disclosable {
        let original = sanitize_diagnostic(
            &failure.describe(),
            &launch.secrets,
            &launch.private_values,
            ORIGINAL_ERROR_BYTE_LIMIT,
        );
        let original = if original.is_empty() {
            failure
                .describe()
                .split(':')
                .next()
                .unwrap_or_default()
                .to_string()
        } else {
            original
        };
        (
            original,
            stderr.tail(&launch.secrets, &launch.private_values),
        )
    } else {
        let name = failure.describe();
        let name = name.split(':').next().unwrap_or_default();
        (
            format!("{name}: details omitted for safe redaction"),
            String::new(),
        )
    };
    let detail = if tail.is_empty() {
        String::new()
    } else {
        format!(" Stderr tail:\n{tail}")
    };
    McpSessionError::new(
        McpErrorKind::Startup,
        format!("MCP stdio server failed during startup ({original}).{detail}"),
    )
}

/// Shared handle to a generation slot's contents.
pub(crate) type SharedGeneration = Arc<tokio::sync::Mutex<Option<Generation>>>;

#[cfg(test)]
mod tests;
