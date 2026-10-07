//! From a server's resolved configuration to what a connection needs: the
//! startup/call deadlines, the stdio launch (command, cwd, the environment
//! the kernel would have passed it) or the HTTP endpoint with its headers,
//! and the credential identity that retires a connection when its token
//! changes. Credentials come from the host's own auth store, endpoint-bound;
//! environment lookups see exactly what the kernel process inherits.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::error::McpSessionError;
use crate::auth::AuthStorage;
use crate::kernel::shared::KernelEnvironment;

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// A stored OAuth access token is refreshed this long before it expires.
const EXPIRY_SKEW_MS: i64 = 30_000;
/// Host variables a stdio server always inherits (when the kernel has them).
const SAFE_ENV: [&str; 7] = [
    "HOME",
    "PATH",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SystemRoot",
    "WINDIR",
];

/// The environment the session's kernel process sees: the host
/// environment under the `kernel.environment` policy, plus the variables the
/// host sets for the kernel. Stdio servers and `bearerTokenEnvVar` lookups
/// read it, so `scrub-credentials` reaches them too.
#[derive(Debug, Clone, Default)]
pub(crate) struct KernelEnv {
    environment: KernelEnvironment,
    overrides: HashMap<String, String>,
    /// Whether host variables count at all (off only in hermetic tests).
    host: bool,
}

impl KernelEnv {
    pub(crate) fn new(environment: KernelEnvironment, overrides: HashMap<String, String>) -> Self {
        Self {
            environment,
            overrides,
            host: true,
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<String> {
        if let Some(value) = self.overrides.get(key) {
            return Some(value.clone());
        }
        if !self.host || !self.environment.passes_to_kernel(key) {
            return None;
        }
        std::env::var(key).ok()
    }
}

/// The host auth store, read fresh on every lookup (a login another process
/// wrote must count at once).
#[derive(Clone)]
pub(crate) struct McpCredentials {
    store: Arc<tokio::sync::Mutex<AuthStorage>>,
    /// Counts a successful refresh as connector use (as `mcp.refresh` does).
    usage: Option<crate::mcp::McpUsageReporter>,
}

impl McpCredentials {
    pub(crate) fn new(
        store: Arc<tokio::sync::Mutex<AuthStorage>>,
        usage: Option<crate::mcp::McpUsageReporter>,
    ) -> Self {
        Self { store, usage }
    }

    /// The raw `mcp:<server>` entry, or `None`.
    async fn read(&self, server: &str) -> Option<Value> {
        let store = Arc::clone(&self.store);
        let provider = super::super::provider_id(server);
        tokio::task::spawn_blocking(move || {
            let mut store = store.blocking_lock();
            store.reload();
            store.get_all().get(&provider).cloned()
        })
        .await
        .ok()
        .flatten()
        .filter(Value::is_object)
    }

    /// Refresh an expiring OAuth login through the store (the `mcp.refresh`
    /// path: a reload, then a key lookup that refreshes an expired token).
    async fn refresh(&self, server: &str) -> bool {
        let store = Arc::clone(&self.store);
        let provider = super::super::provider_id(server);
        let refreshed = tokio::task::spawn_blocking(move || {
            let mut store = store.blocking_lock();
            store.reload();
            store.get_api_key(&provider).is_some()
        })
        .await
        .unwrap_or(false);
        if refreshed {
            if let Some(report) = &self.usage {
                report("refresh", server);
            }
        }
        refreshed
    }

    /// The stored credential, only when bound to exactly this endpoint: a
    /// token bound elsewhere (a login that finished before a retarget) is
    /// never attached.
    async fn bound(&self, server: &str, config: &Value) -> Option<Value> {
        let credential = self.read(server).await?;
        let endpoint = credential.get("endpoint").and_then(Value::as_str)?;
        let url = config
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        (endpoint == url).then_some(credential)
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn credential_source(config: &Value) -> Option<&str> {
    str_field(config, "credentialSource")
}

fn uses_oauth(config: &Value) -> bool {
    config.get("oauth").and_then(Value::as_bool) == Some(true)
}

/// A stored `api_key` value: a literal, an env-var name, or a `!command`
/// (which never runs here: those resolve host-side or not at all).
fn resolve_config_value(value: &str, env: &KernelEnv) -> String {
    let value = value.trim();
    if value.is_empty() || value.starts_with('!') {
        return String::new();
    }
    env.get(value)
        .filter(|resolved| !resolved.is_empty())
        .unwrap_or_else(|| value.to_string())
        .trim()
        .to_string()
}

fn bearer_env_token(config: &Value, env: &KernelEnv) -> (Option<String>, String) {
    let name = str_field(config, "bearerTokenEnvVar")
        .filter(|name| !name.is_empty())
        .map(ToString::to_string);
    let token = name
        .as_deref()
        .and_then(|name| env.get(name))
        .map(|value| value.trim().to_string())
        .unwrap_or_default();
    (name, token)
}

fn stored_token(credential: Option<&Value>, env: &KernelEnv) -> String {
    let raw = credential
        .and_then(|credential| {
            str_field(credential, "access")
                .filter(|access| !access.is_empty())
                .or_else(|| str_field(credential, "key"))
        })
        .unwrap_or_default();
    resolve_config_value(raw, env)
}

/// The literal pasted bearer of a `static-token` connection (endpoint-bound;
/// never env- or command-resolved).
async fn static_token(server: &str, config: &Value, credentials: &McpCredentials) -> String {
    credentials
        .bound(server, config)
        .await
        .as_ref()
        .and_then(|credential| str_field(credential, "bearer"))
        .map(|bearer| bearer.trim().to_string())
        .unwrap_or_default()
}

fn sha256_hex(token: &str) -> String {
    use std::fmt::Write as _;
    Sha256::digest(token.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(i64::MAX, |elapsed| elapsed.as_millis() as i64)
}

/// The credential identity of an HTTP connection (a hash of the token it
/// would send, or `anonymous`). Part of the connection's fingerprint, so a
/// rotated token reconnects. Refreshes an OAuth login about to expire.
///
/// # Errors
///
/// `McpCredentialsUnavailable` when the connection needs a credential it
/// does not have; `RuntimeError` when a needed refresh fails.
pub(crate) async fn auth_identity(
    server: &str,
    config: &Value,
    env: &KernelEnv,
    credentials: &McpCredentials,
) -> Result<String, McpSessionError> {
    if credential_source(config) == Some("static-token") {
        let token = static_token(server, config, credentials).await;
        if token.is_empty() {
            return Err(McpSessionError::credentials_unavailable(server));
        }
        return Ok(sha256_hex(&token));
    }
    let (env_name, mut token) = bearer_env_token(config, env);
    if uses_oauth(config) && token.is_empty() {
        let mut credential = credentials.bound(server, config).await;
        let expires = credential
            .as_ref()
            .and_then(|credential| credential.get("expires"))
            .and_then(Value::as_f64);
        if expires.is_some_and(|expires| expires <= (now_ms() + EXPIRY_SKEW_MS) as f64) {
            if !credentials.refresh(server).await {
                return Err(McpSessionError::runtime(format!(
                    "Could not refresh MCP credentials for '{server}'"
                )));
            }
            credential = credentials.bound(server, config).await;
        }
        token = stored_token(credential.as_ref(), env);
    }
    if token.is_empty() {
        if uses_oauth(config) || env_name.is_some() {
            return Err(McpSessionError::credentials_unavailable(server));
        }
        return Ok("anonymous".to_string());
    }
    Ok(sha256_hex(&token))
}

/// The headers an HTTP connection sends: the configured ones, then the
/// Authorization header its credential source yields (which wins).
///
/// # Errors
///
/// `ValueError` for non-string headers; `McpCredentialsUnavailable` when the
/// connection needs a credential it does not have.
pub(crate) async fn http_headers(
    server: &str,
    config: &Value,
    env: &KernelEnv,
    credentials: &McpCredentials,
) -> Result<Vec<(String, String)>, McpSessionError> {
    let mut headers = string_map(
        config.get("headers"),
        "MCP HTTP headers must contain strings",
    )?;
    if credential_source(config) == Some("acp") {
        return Ok(headers);
    }
    let token = if credential_source(config) == Some("static-token") {
        let token = static_token(server, config, credentials).await;
        if token.is_empty() {
            return Err(McpSessionError::credentials_unavailable(server));
        }
        token
    } else {
        let (env_name, mut token) = bearer_env_token(config, env);
        if uses_oauth(config) && token.is_empty() {
            let credential = credentials.bound(server, config).await;
            token = stored_token(credential.as_ref(), env);
        }
        if token.is_empty() {
            if uses_oauth(config) || env_name.is_some() {
                return Err(McpSessionError::credentials_unavailable(server));
            }
            return Ok(headers);
        }
        token
    };
    headers.retain(|(name, _)| name != "Authorization");
    headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    Ok(headers)
}

/// The configured `headers` of `config`, verbatim.
///
/// # Errors
///
/// `ValueError` for non-string headers.
pub(crate) fn string_headers(config: &Value) -> Result<Vec<(String, String)>, McpSessionError> {
    string_map(
        config.get("headers"),
        "MCP HTTP headers must contain strings",
    )
}

/// A JSON object of string values (absent is empty), in key order.
fn string_map(
    value: Option<&Value>,
    message: &str,
) -> Result<Vec<(String, String)>, McpSessionError> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, value)| {
                value
                    .as_str()
                    .map(|value| (key.clone(), value.to_string()))
                    .ok_or_else(|| McpSessionError::value(message))
            })
            .collect(),
        Some(_) => Err(McpSessionError::value(message)),
    }
}

/// The startup and per-call deadlines (`startupTimeoutMs`, `callTimeoutMs`).
///
/// # Errors
///
/// `ValueError` for a non-positive or non-numeric value.
pub(crate) fn timeouts(config: &Value) -> Result<(Duration, Duration), McpSessionError> {
    Ok((
        milliseconds(config.get("startupTimeoutMs"), DEFAULT_STARTUP_TIMEOUT)?,
        milliseconds(config.get("callTimeoutMs"), DEFAULT_CALL_TIMEOUT)?,
    ))
}

fn milliseconds(value: Option<&Value>, default: Duration) -> Result<Duration, McpSessionError> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|millis| *millis > 0.0 && millis.is_finite())
            .map(|millis| Duration::from_secs_f64(millis / 1000.0))
            .ok_or_else(|| McpSessionError::value("MCP timeouts must be positive milliseconds")),
        Some(_) => Err(McpSessionError::value(
            "MCP timeouts must be positive milliseconds",
        )),
    }
}

/// How to launch a stdio server, plus what its diagnostics must redact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StdioLaunch {
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) env: Vec<(String, String)>,
    /// The configured environment values, longest first: never shown.
    pub(crate) secrets: Vec<String>,
    /// False when a configured value is too short to redact safely: the
    /// startup diagnostic then shows no child output at all.
    pub(crate) disclosable: bool,
    /// Configuration strings an SDK error must not echo, longest first.
    pub(crate) private_values: Vec<String>,
}

/// The stdio launch for `config`, run in `session_cwd` unless it names a cwd.
///
/// # Errors
///
/// `ValueError` for a malformed command, args, cwd, or env.
pub(crate) fn stdio_launch(
    server: &str,
    config: &Value,
    env: &KernelEnv,
    session_cwd: &Path,
) -> Result<StdioLaunch, McpSessionError> {
    let command = str_field(config, "command").filter(|command| !command.is_empty());
    let args = match config.get("args") {
        None => Some(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(ToString::to_string))
            .collect::<Option<Vec<_>>>(),
        Some(_) => None,
    };
    let (Some(command), Some(args)) = (command, args) else {
        return Err(McpSessionError::value(format!(
            "MCP server '{server}' requires command and string args"
        )));
    };
    let cwd = match config.get("cwd") {
        None | Some(Value::Null) => session_cwd.to_path_buf(),
        Some(Value::String(cwd)) => PathBuf::from(cwd),
        Some(_) => {
            return Err(McpSessionError::value(format!(
                "MCP server '{server}' cwd must be a string"
            )));
        }
    };
    let mut launch_env: Vec<(String, String)> = SAFE_ENV
        .iter()
        .filter_map(|key| env.get(key).map(|value| ((*key).to_string(), value)))
        .collect();
    let configured = stdio_env(config, env)?;
    let configured_values: Vec<String> =
        configured.iter().map(|(_, value)| value.clone()).collect();
    for (key, value) in configured {
        launch_env.retain(|(existing, _)| existing != &key);
        launch_env.push((key, value));
    }
    let disclosable = !configured_values
        .iter()
        .any(|value| (1..4).contains(&value.chars().count()));
    let secrets = longest_first(
        configured_values
            .into_iter()
            .filter(|value| value.chars().count() >= 4),
    );
    let mut private = BTreeSet::new();
    for key in [
        "command",
        "args",
        "cwd",
        "url",
        "headers",
        "env",
        "bearerTokenEnvVar",
    ] {
        if let Some(value) = config.get(key) {
            collect_strings(value, &mut private);
        }
    }
    private.insert(session_cwd.to_string_lossy().to_string());
    Ok(StdioLaunch {
        command: command.to_string(),
        args,
        cwd,
        env: launch_env,
        secrets,
        disclosable,
        private_values: longest_first(private.into_iter().filter(|value| !value.is_empty())),
    })
}

/// The configured stdio environment: `{"env": "NAME"}` references resolved
/// against the kernel environment, or literal values for an ACP server.
fn stdio_env(config: &Value, env: &KernelEnv) -> Result<Vec<(String, String)>, McpSessionError> {
    let raw = match config.get("env") {
        None => return Ok(Vec::new()),
        Some(Value::Object(map)) => map,
        Some(_) => return Err(McpSessionError::value("MCP stdio env must be an object")),
    };
    if credential_source(config) == Some("acp") {
        return string_map(
            config.get("env"),
            "ACP MCP stdio env must contain string values",
        );
    }
    raw.iter()
        .map(|(key, reference)| {
            let source = match reference {
                Value::Object(reference) if reference.len() == 1 => reference.get("env"),
                _ => None,
            }
            .ok_or_else(|| {
                McpSessionError::value(
                    "MCP stdio env values must use {\"env\": \"NAME\"} references",
                )
            })?;
            source
                .as_str()
                .and_then(|source| env.get(source))
                .map(|value| (key.clone(), value))
                .ok_or_else(|| {
                    McpSessionError::value(format!(
                        "MCP stdio environment reference for '{key}' is unavailable"
                    ))
                })
        })
        .collect()
}

fn collect_strings(value: &Value, into: &mut BTreeSet<String>) {
    match value {
        Value::String(text) => {
            into.insert(text.clone());
        }
        Value::Object(map) => {
            for (key, item) in map {
                into.insert(key.clone());
                collect_strings(item, into);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_strings(item, into);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn longest_first(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values: Vec<String> = values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    values.sort_by_key(|value| std::cmp::Reverse(value.chars().count()));
    values
}

/// The HTTP endpoint of `config`.
///
/// # Errors
///
/// `ValueError` when the URL is missing.
pub(crate) fn http_url(server: &str, config: &Value) -> Result<String, McpSessionError> {
    str_field(config, "url")
        .filter(|url| !url.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| McpSessionError::value(format!("MCP server '{server}' requires a URL")))
}

/// Python's `repr` of a transport kind, for the unsupported-transport error.
pub(crate) fn transport_repr(kind: Option<&Value>) -> String {
    match kind {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(kind)) => format!("'{kind}'"),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests;
