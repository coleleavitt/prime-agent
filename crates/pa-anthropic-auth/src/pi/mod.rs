//! The pi plugin's request for the store's tokens (anthropic-auth
//! `packages/pi` `stream.ts`, `sendAnthropicRequestUnrecorded`, and the
//! `convert.ts` it builds bodies with): pa-ai's request hooks hand over
//! the caller's conversation (`RequestSource`) and the hook builds pi's
//! body from it, then pi's headers (`shape.rs`).

use std::path::PathBuf;

use anthropic::cch::js_json_stringify;
use anthropic::claude_code::{order_claude_code_body, FAST_MODE_BETA, SERVER_SIDE_FALLBACK_BETAS};
use pa_ai::request_hooks::{OutgoingRequest, RequestSource};
use serde_json::Value;

use crate::shape::{shape_headers, ShapeEnv, ShapeIdentity};
use crate::SharedStoreSource;

pub(crate) mod account_commands;
pub(crate) mod commands;
pub(crate) mod context1m;
pub(crate) mod convert;
pub(crate) mod fallback;
pub(crate) mod settings;

use convert::RequestSettings;
use settings::PluginSettings;

/// Where the plugin's request path reads and writes.
#[derive(Debug, Clone)]
pub struct PiConfig {
    /// The plugin's settings file (`anthropic-auth.json` in pi's agent
    /// directory).
    pub settings_path: PathBuf,
}

impl PiConfig {
    /// The paths the plugin resolves from the same environment. No I/O.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            settings_path: settings::settings_path_from_env(),
        }
    }

    /// Everything under `directory` (tests and sandboxes).
    #[must_use]
    pub fn under(directory: &std::path::Path) -> Self {
        Self {
            settings_path: directory.join(settings::SETTINGS_FILE),
        }
    }
}

/// The request path's state in this process.
#[derive(Debug)]
pub(crate) struct PiRequests {
    pub(crate) settings: PluginSettings,
    /// The tokens latched to the standard context window.
    pub(crate) context1m: context1m::Context1mLatch,
}

impl PiRequests {
    pub(crate) fn new(config: &PiConfig) -> Self {
        Self {
            settings: PluginSettings::new(config.settings_path.clone()),
            context1m: context1m::Context1mLatch::default(),
        }
    }
}

/// What pi sends for a request: the body (and the payload it stands for)
/// and the betas it merges into the header's tuple.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outgoing {
    pub(crate) payload: Value,
    pub(crate) text: String,
    pub(crate) extra_betas: Vec<&'static str>,
}

/// What a request is built from besides the caller's conversation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BuildInputs<'a> {
    pub(crate) settings: RequestSettings,
    pub(crate) identity: &'a ShapeIdentity,
    pub(crate) version: &'a str,
    /// `CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING`.
    pub(crate) disable_adaptive_flag: Option<&'a str>,
}

/// pi's request for `model` over the caller's request
/// (`sendAnthropicRequestUnrecorded` up to the headers): the built body,
/// server-side fallback and its stored markers, then the bytes: Claude
/// Code's key order when nothing changed the built body, else the changed
/// body as it stands.
pub(crate) fn build_outgoing(
    model: &str,
    source: &RequestSource<'_>,
    inputs: &BuildInputs<'_>,
) -> Outgoing {
    let built = convert::build_request(
        model,
        source,
        inputs.settings,
        inputs.identity,
        inputs.version,
        inputs.disable_adaptive_flag,
    );
    let mut body = built.body;
    let server_fallback = fallback::is_fallback_model(model);
    if server_fallback {
        if let Some(map) = body.as_object_mut() {
            map.insert(
                "fallbacks".to_string(),
                Value::String("default".to_string()),
            );
        }
    }
    let markers_changed = fallback::rewrite_stored_markers(&mut body, server_fallback);
    let mut extra_betas: Vec<&'static str> = Vec::new();
    if server_fallback {
        extra_betas.extend(SERVER_SIDE_FALLBACK_BETAS);
    }
    if body.get("speed").and_then(Value::as_str) == Some("fast") {
        extra_betas.push(FAST_MODE_BETA);
    }
    if server_fallback || markers_changed {
        let text = js_json_stringify(&body);
        return Outgoing {
            payload: body,
            text,
            extra_betas,
        };
    }
    Outgoing {
        payload: order_claude_code_body(body),
        text: built.body_text,
        extra_betas,
    }
}

/// Send `request` as pi does: its body, then its headers.
pub(crate) fn prepare(
    source: &SharedStoreSource,
    request: &mut OutgoingRequest<'_>,
    identity: &ShapeIdentity,
) {
    let version = source.claude_code_version();
    let flag = std::env::var(anthropic::models::DISABLE_ADAPTIVE_THINKING_ENV).ok();
    let outgoing = build_outgoing(
        &request.model.id,
        &request.source,
        &BuildInputs {
            settings: source.pi.settings.request(),
            identity,
            version: &version,
            disable_adaptive_flag: flag.as_deref(),
        },
    );
    *request.payload = outgoing.payload;
    *request.body = Some(outgoing.text);
    shape_headers(
        request,
        identity,
        &version,
        &ShapeEnv::from_env(),
        &uuid::Uuid::new_v4().to_string(),
        &outgoing.extra_betas,
        source.pi.context1m.is_clamped(request.api_key),
    );
}

/// The events pa-ai reads for one streamed event of a store-served
/// response: a `fallback` block kept as pi's marker, pi's tool-name alias
/// restored (`fromClaudeCodeToolName`; pa-ai matches the rest of the name
/// to the caller's tools as pi does).
pub(crate) fn response_event(mut event: Value) -> Vec<Value> {
    if let Some(events) = fallback::marker_events(&event) {
        return events;
    }
    if let Some(block) = event
        .get_mut("content_block")
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
    {
        if block.get("name").and_then(Value::as_str) == Some(convert::DEEP_RESEARCH_WIRE_TOOL) {
            block["name"] = Value::String(convert::DEEP_RESEARCH_TOOL.to_string());
        }
    }
    vec![event]
}

#[cfg(test)]
mod tests;
