//! The pi plugin's request for the store's tokens (anthropic-auth
//! `packages/pi` `stream.ts`, `sendAnthropicRequestUnrecorded`, and the
//! `convert.ts` it builds bodies with): pa-ai's request hooks hand over
//! the caller's conversation (`RequestSource`) and the hook builds pi's
//! body from it, then pi's headers (`shape.rs`).

use std::path::PathBuf;

use pa_ai::request_hooks::OutgoingRequest;
use serde_json::Value;

use crate::shape::{shape_headers, ShapeEnv, ShapeIdentity};
use crate::SharedStoreSource;

pub(crate) mod convert;
pub(crate) mod settings;

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
}

impl PiRequests {
    pub(crate) fn new(config: &PiConfig) -> Self {
        Self {
            settings: PluginSettings::new(config.settings_path.clone()),
        }
    }
}

/// The body pi sends for `request`, and the payload it stands for.
pub(crate) struct OutgoingBody {
    pub(crate) payload: Value,
    pub(crate) text: String,
}

/// pi's body for the request (`sendAnthropicRequestUnrecorded` up to the
/// serialized body).
pub(crate) fn outgoing_body(
    source: &SharedStoreSource,
    request: &OutgoingRequest<'_>,
    identity: &ShapeIdentity,
    version: &str,
) -> OutgoingBody {
    let settings = source.pi.settings.request();
    let flag = std::env::var(anthropic::models::DISABLE_ADAPTIVE_THINKING_ENV).ok();
    let built = convert::build_request(
        &request.model.id,
        &request.source,
        settings,
        identity,
        version,
        flag.as_deref(),
    );
    OutgoingBody {
        payload: anthropic::claude_code::order_claude_code_body(built.body),
        text: built.body_text,
    }
}

/// Send `request` as pi does: its body, then its headers.
pub(crate) fn prepare(
    source: &SharedStoreSource,
    request: &mut OutgoingRequest<'_>,
    identity: &ShapeIdentity,
) {
    let version = source.claude_code_version();
    let body = outgoing_body(source, request, identity, &version);
    *request.payload = body.payload;
    *request.body = Some(body.text);
    shape_headers(
        request,
        identity,
        &version,
        &ShapeEnv::from_env(),
        &uuid::Uuid::new_v4().to_string(),
    );
}

#[cfg(test)]
mod tests;
