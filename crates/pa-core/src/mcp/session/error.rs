//! The typed failure of one MCP session operation, carried to the kernel as
//! data so `rlm.mcp` raises the same exception class (and message) the
//! in-kernel client raised.

use serde_json::{json, Value};

/// The Python exception class the kernel raises for a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpErrorKind {
    /// `RuntimeError`: a disabled server, a failed refresh, a transport or
    /// protocol failure.
    Runtime,
    /// `KeyError`: an undeclared server, or a tool the server does not have.
    Key,
    /// `PermissionError`: `enabledTools` / `disabledTools` excludes the tool.
    Permission,
    /// `ValueError`: a malformed server configuration.
    Value,
    /// `TimeoutError`: the startup or call deadline passed.
    Timeout,
    /// `FileNotFoundError`: a stdio command that does not exist.
    FileNotFound,
    /// `OSError`: any other failure to start a stdio command.
    Os,
    /// `McpStartupError`: a stdio server failed its startup handshake.
    Startup,
    /// `McpDiscoveryError`: `tools/list` pagination cannot complete honestly.
    Discovery,
    /// `McpCredentialsUnavailable`: the connection has no usable credentials.
    CredentialsUnavailable,
    /// `McpToolError`: the server flagged the call result as an error.
    Tool,
    /// `asyncio.CancelledError`: the kernel cancelled the request.
    Cancelled,
}

impl McpErrorKind {
    fn python_name(self) -> &'static str {
        match self {
            McpErrorKind::Runtime => "RuntimeError",
            McpErrorKind::Key => "KeyError",
            McpErrorKind::Permission => "PermissionError",
            McpErrorKind::Value => "ValueError",
            McpErrorKind::Timeout => "TimeoutError",
            McpErrorKind::FileNotFound => "FileNotFoundError",
            McpErrorKind::Os => "OSError",
            McpErrorKind::Startup => "McpStartupError",
            McpErrorKind::Discovery => "McpDiscoveryError",
            McpErrorKind::CredentialsUnavailable => "McpCredentialsUnavailable",
            McpErrorKind::Tool => "McpToolError",
            McpErrorKind::Cancelled => "CancelledError",
        }
    }
}

/// One failed MCP operation: the exception class and its exact message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", kind.python_name())]
pub(crate) struct McpSessionError {
    pub(crate) kind: McpErrorKind,
    pub(crate) message: String,
}

impl McpSessionError {
    pub(crate) fn new(kind: McpErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn runtime(message: impl Into<String>) -> Self {
        Self::new(McpErrorKind::Runtime, message)
    }

    pub(crate) fn value(message: impl Into<String>) -> Self {
        Self::new(McpErrorKind::Value, message)
    }

    /// The in-kernel client's `TimeoutError` carried no message.
    pub(crate) fn timeout() -> Self {
        Self::new(McpErrorKind::Timeout, "")
    }

    pub(crate) fn cancelled() -> Self {
        Self::new(McpErrorKind::Cancelled, "")
    }

    /// `KeyError` for a tool the server does not list.
    pub(crate) fn no_tool(server: &str, tool: &str) -> Self {
        Self::new(
            McpErrorKind::Key,
            format!("MCP server '{server}' has no tool '{tool}'"),
        )
    }

    /// `PermissionError` for a tool the connection's policy excludes.
    pub(crate) fn tool_disabled(server: &str, tool: &str) -> Self {
        Self::new(
            McpErrorKind::Permission,
            format!("MCP tool '{tool}' is disabled for server '{server}'"),
        )
    }

    pub(crate) fn credentials_unavailable(server: &str) -> Self {
        Self::new(
            McpErrorKind::CredentialsUnavailable,
            format!(
                "MCP credentials for '{server}' are not available. Ask the user to connect it \
                 (/plugins or /mcp login {server}); do not ask them to set environment variables."
            ),
        )
    }

    /// The Python class name the in-kernel client would have reported for
    /// this failure in a diagnostic (`f"{type(exc).__name__}: {exc}"`).
    pub(crate) fn python_name(&self) -> &'static str {
        self.kind.python_name()
    }

    /// The wire form: `{"type": <class>, "message": <text>}`.
    pub(crate) fn to_wire(&self) -> Value {
        json!({ "type": self.kind.python_name(), "message": self.message })
    }
}
