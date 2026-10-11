//! The pi plugin's request shape for an OAuth Messages request the store's
//! token authenticates (anthropic-auth `packages/core` `claude-code.ts` /
//! `cch.ts` / `claude-version.ts`, as pi's `buildAnthropicRequest` and
//! `sendAnthropicRequestUnrecorded` apply them):
//!
//! - the headers `applyClaudeCodeHeaders` sets on a fresh request: the
//!   Claude Code beta tuple chosen by body shape (plus `fast-mode` for
//!   `speed:"fast"`, `context-1m` for a 1M-capable model, then the request's
//!   own betas), the `claude-cli/<version> (external, <entrypoint>)` user
//!   agent, the Claude Code `x-stainless-*` set, `x-claude-code-session-id`
//!   and a fresh `x-client-request-id`, the environment-forwarded headers;
//! - the pieces of pi's body built in `pi/convert.rs`: the billing block
//!   (`x-anthropic-billing-header: cc_version=<version>.<suffix>;
//!   cc_entrypoint=cli; cch=00000;`) and `metadata.user_id` (device id,
//!   account uuid, session id; only when the account uuid is known).
//!
//! Pure functions over the request; the identity and the version are the
//! caller's (see `hooks.rs`). Golden: `tests/fixtures/golden/`.

use anthropic::claude_code::{
    CLAUDE_CODE_STAINLESS_PACKAGE_VERSION,
    CLAUDE_CODE_STAINLESS_RUNTIME_VERSION,
    CONTEXT_1M_BETA,
    EFFORT_BETA,
    FAST_MODE_BETA,
    encode_header_value,
    stainless_arch,
    stainless_os,
};
use anthropic::claude_version::{UserAgentDetails, claude_code_user_agent};
use anthropic::models::model_supports_context_1m;
use serde_json::Value;

/// The plugin's base tuple (`CLAUDE_CODE_BASE_BETAS`).
const BASE_BETAS: [&str; 9] = [
    "claude-code-20250219",
    "oauth-2025-04-20",
    "interleaved-thinking-2025-05-14",
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    EFFORT_BETA,
    "extended-cache-ttl-2025-04-11",
    "cache-diagnosis-2026-04-07",
];
/// What the full-agent tuple adds to the base one.
const FULL_AGENT_BETAS: [&str; 2] = ["advisor-tool-2026-03-01", "advanced-tool-use-2025-11-20"];
/// What the structured-output tuple adds to the base one.
const STRUCTURED_OUTPUT_BETA: &str = "structured-outputs-2025-12-15";
/// The billing block's entrypoint (`CLAUDE_CODE_ENTRYPOINT`).
const BILLING_ENTRYPOINT: &str = "cli";
/// Character positions of the first user text the version suffix samples.
const SUFFIX_POSITIONS: [usize; 3] = [4, 7, 20];
/// The suffix salt (`CCH_SALT`).
const SUFFIX_SALT: &str = "59cf53e54c78";
/// Headers the plugin forwards from the environment, `(header, variable)`.
const ENV_FORWARDED_HEADERS: [(&str, &str); 3] = [
    ("x-claude-remote-container-id", "CLAUDE_CODE_CONTAINER_ID"),
    (
        "x-claude-remote-session-id",
        "CLAUDE_CODE_REMOTE_SESSION_ID",
    ),
    ("x-client-app", "CLAUDE_AGENT_SDK_CLIENT_APP"),
];

/// Who the request says it comes from (pi's `ClaudeCodeIdentity`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShapeIdentity {
    /// The installation's device id (`~/.anthropic-accounts/device.json`);
    /// `None` when it could not be read or created.
    pub(crate) device_id: Option<String>,
    /// The account's uuid, when the store knows it.
    pub(crate) account_uuid: Option<String>,
    /// The per-account session id of this process.
    pub(crate) session_id: String,
}

/// What the environment adds (the plugin reads `process.env` per request).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ShapeEnv {
    pub(crate) entrypoint: Option<String>,
    pub(crate) sdk_version: Option<String>,
    pub(crate) client_app: Option<String>,
    pub(crate) forwarded: Vec<(String, String)>,
    pub(crate) additional_protection: bool,
}

impl ShapeEnv {
    /// The values `lookup` (an environment reader) holds.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            entrypoint: lookup("CLAUDE_CODE_ENTRYPOINT"),
            sdk_version: lookup("CLAUDE_AGENT_SDK_VERSION"),
            client_app: lookup("CLAUDE_AGENT_SDK_CLIENT_APP"),
            forwarded: ENV_FORWARDED_HEADERS
                .iter()
                .filter_map(|(header, variable)| {
                    lookup(variable)
                        .filter(|value| !value.is_empty())
                        .map(|value| ((*header).to_string(), encode_header_value(&value)))
                })
                .collect(),
            additional_protection: lookup("CLAUDE_CODE_ADDITIONAL_PROTECTION").is_some_and(
                |value| {
                    ["1", "true", "yes", "on"].contains(&value.trim().to_ascii_lowercase().as_str())
                },
            ),
        }
    }

    /// The process environment's values.
    pub(crate) fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// The plugin's user agent for `version` (`getClaudeCodeUserAgent`).
    pub(crate) fn user_agent(&self, version: &str) -> String {
        claude_code_user_agent(
            version,
            &UserAgentDetails {
                entrypoint: self.entrypoint.as_deref(),
                sdk_version: self.sdk_version.as_deref(),
                client_app: self.client_app.as_deref(),
            },
        )
    }
}

fn is_record(value: Option<&Value>) -> bool {
    value.is_some_and(Value::is_object)
}

/// The plugin's `selectClaudeCodeBetas`: the tuple by body shape, then
/// `fast-mode`, `context-1m`, and `extra`, first occurrence kept.
pub(crate) fn select_betas(
    body: Option<&Value>,
    extra: &[&str],
    suppress_context_1m: bool,
) -> String {
    let mut selected: Vec<&str> = BASE_BETAS.to_vec();
    if let Some(body) = body {
        let full_agent = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty())
            && body.get("system").is_some_and(Value::is_array)
            && is_record(body.get("thinking"))
            && is_record(body.get("context_management"))
            && is_record(body.get("output_config"))
            && is_record(body.get("diagnostics"));
        let structured = body
            .pointer("/output_config/format/type")
            .and_then(Value::as_str)
            == Some("json_schema");
        if full_agent {
            selected.extend(FULL_AGENT_BETAS);
        } else if structured {
            selected.push(STRUCTURED_OUTPUT_BETA);
        }
        if body.get("speed").and_then(Value::as_str) == Some("fast") {
            selected.push(FAST_MODE_BETA);
        }
        if !suppress_context_1m
            && body
                .get("model")
                .and_then(Value::as_str)
                .is_some_and(model_supports_context_1m)
        {
            selected.push(CONTEXT_1M_BETA);
        }
    }
    selected.extend(
        extra
            .iter()
            .map(|beta| beta.trim())
            .filter(|beta| !beta.is_empty()),
    );
    let mut unique: Vec<&str> = Vec::with_capacity(selected.len());
    for beta in selected {
        if !unique.contains(&beta) {
            unique.push(beta);
        }
    }
    unique.join(",")
}

/// The header set the plugin builds on a fresh request
/// (`applyClaudeCodeHeaders`), in the order it sets them; `incoming_betas`
/// are the request's own (merged after the tuple).
pub(crate) fn claude_code_headers(
    token: &str,
    body: &Value,
    identity: &ShapeIdentity,
    version: &str,
    env: &ShapeEnv,
    incoming_betas: &str,
    request_id: &str,
) -> Vec<(String, String)> {
    let extra: Vec<&str> = incoming_betas
        .split(',')
        .map(str::trim)
        .filter(|beta| !beta.is_empty())
        .collect();
    let mut headers: Vec<(String, String)> = [
        ("accept", "application/json".to_string()),
        ("authorization", format!("Bearer {token}")),
        ("content-type", "application/json".to_string()),
        ("user-agent", env.user_agent(version)),
        ("anthropic-beta", select_betas(Some(body), &extra, false)),
        (
            "anthropic-dangerous-direct-browser-access",
            "true".to_string(),
        ),
        ("anthropic-version", "2023-06-01".to_string()),
        ("x-app", "cli".to_string()),
        ("x-client-request-id", request_id.to_string()),
        ("x-claude-code-session-id", identity.session_id.clone()),
        ("x-stainless-arch", stainless_arch().to_string()),
        ("x-stainless-lang", "js".to_string()),
        ("x-stainless-os", stainless_os().to_string()),
        (
            "x-stainless-package-version",
            CLAUDE_CODE_STAINLESS_PACKAGE_VERSION.to_string(),
        ),
        ("x-stainless-retry-count", "0".to_string()),
        ("x-stainless-runtime", "node".to_string()),
        (
            "x-stainless-runtime-version",
            CLAUDE_CODE_STAINLESS_RUNTIME_VERSION.to_string(),
        ),
        ("x-stainless-timeout", "600".to_string()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value))
    .collect();
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        headers.push((
            "x-stainless-helper-method".to_string(),
            "stream".to_string(),
        ));
    }
    headers.extend(env.forwarded.iter().cloned());
    if env.additional_protection {
        headers.push((
            "x-anthropic-additional-protection".to_string(),
            "true".to_string(),
        ));
    }
    headers
}

/// The first non-meta user message's text: a string body, else its first
/// text block (the plugin's `extractFirstUserMessageText`).
pub(crate) fn first_user_text(messages: &[Value]) -> String {
    let Some(user) = messages.iter().find(|message| {
        message.get("role").and_then(Value::as_str) == Some("user")
            && message.get("isMeta").and_then(Value::as_bool) != Some(true)
    }) else {
        return String::new();
    };
    match user.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|block| block.get("text").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// The billing block's text for `messages` (`buildBillingHeaderValue` with
/// no attribution): the version suffix samples UTF-16 positions 4, 7 and 20
/// of the first user text (`0` past its end).
pub(crate) fn billing_text(messages: &[Value], version: &str) -> String {
    use sha2::{Digest, Sha256};
    let units: Vec<u16> = first_user_text(messages).encode_utf16().collect();
    let sampled: String = SUFFIX_POSITIONS
        .iter()
        .map(|position| {
            units.get(*position).map_or_else(
                || "0".to_string(),
                |unit| String::from_utf16_lossy(&[*unit]),
            )
        })
        .collect();
    let digest = Sha256::digest(format!("{SUFFIX_SALT}{sampled}{version}").as_bytes());
    let suffix = &format!("{:02x}{:02x}", digest[0], digest[1])[..3];
    format!(
        "x-anthropic-billing-header: cc_version={version}.{suffix}; cc_entrypoint={BILLING_ENTRYPOINT}; cch=00000;"
    )
}

/// `metadata.user_id` (`buildClaudeCodeMetadataUserId`): `None` without an
/// account uuid (or a device id).
pub(crate) fn metadata_user_id(identity: &ShapeIdentity) -> Option<String> {
    Some(anthropic::claude_code::build_claude_code_metadata_user_id(
        identity.device_id.as_deref()?,
        identity.account_uuid.as_deref()?,
        &identity.session_id,
    ))
}

/// Header names the shape replaces (compared case-insensitively), besides
/// `x-api-key`, which an OAuth request never carries.
fn replaced(name: &str) -> bool {
    const SET: [&str; 23] = [
        "accept",
        "authorization",
        "content-type",
        "user-agent",
        "anthropic-beta",
        "anthropic-dangerous-direct-browser-access",
        "anthropic-version",
        "x-app",
        "x-client-request-id",
        "x-claude-code-session-id",
        "x-stainless-arch",
        "x-stainless-lang",
        "x-stainless-os",
        "x-stainless-package-version",
        "x-stainless-retry-count",
        "x-stainless-runtime",
        "x-stainless-runtime-version",
        "x-stainless-timeout",
        "x-stainless-helper-method",
        "x-claude-remote-container-id",
        "x-claude-remote-session-id",
        "x-client-app",
        "x-anthropic-additional-protection",
    ];
    name.eq_ignore_ascii_case("x-api-key")
        || SET.iter().any(|known| name.eq_ignore_ascii_case(known))
}

/// pi's headers for an outgoing request (a fresh `Headers` through
/// `applyClaudeCodeHeaders`, so none of the request's own betas;
/// `context-1m` suppressed for a latched token; pi's `extra_betas` merged
/// after the tuple), ahead of
/// the request's other headers (the ones the shape does not set, kept in
/// order: a provider's configured headers).
pub(crate) fn shape_headers(
    request: &mut pa_ai::request_hooks::OutgoingRequest<'_>,
    identity: &ShapeIdentity,
    version: &str,
    env: &ShapeEnv,
    request_id: &str,
    extra_betas: &[&str],
    suppress_context_1m: bool,
) {
    let mut headers = claude_code_headers(
        request.api_key,
        request.payload,
        identity,
        version,
        env,
        "",
        request_id,
    );
    // pi merges its own betas into the tuple (`mergeAnthropicBetas`).
    if let Some((_, betas)) = headers
        .iter_mut()
        .find(|(name, _)| name == "anthropic-beta")
    {
        // The credits latch: the tuple without `context-1m`.
        let tuple = if suppress_context_1m {
            select_betas(Some(request.payload), &[], true)
        } else {
            std::mem::take(betas)
        };
        *betas = anthropic::claude_code::merge_anthropic_betas(&tuple, extra_betas);
    }
    headers.extend(
        request
            .headers
            .drain(..)
            .filter(|(name, _)| !replaced(name)),
    );
    *request.headers = headers;
}

#[cfg(test)]
mod tests;
