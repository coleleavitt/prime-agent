//! Model resolution and the one-shot completions the feature runs: the
//! Anthropic search call and the `web.fetch` prompt answer. Credentials
//! resolve through the session's model registry (auth.json, the
//! environment, and any installed credential source such as the shared
//! Anthropic account store), the way every other model call does.

use std::collections::BTreeMap;
use std::path::Path;

use pa_ai::types::{OnEventHook, OnPayloadHook, SimpleStreamOptions, StreamOptions};
use pa_types::ai::{
    AssistantContentBlock,
    AssistantMessage,
    Context,
    Message,
    Model,
    StopReason,
    UserContent,
    UserMessage,
};
use tokio_util::sync::CancellationToken;

/// The API whose models can run Anthropic's server-side tools.
pub const ANTHROPIC_MESSAGES_API: &str = "anthropic-messages";
/// Anthropic's first-party provider id.
pub const ANTHROPIC_PROVIDER: &str = "anthropic";

/// A model with the credential and headers its requests carry.
#[derive(Clone)]
pub struct ResolvedModel {
    pub model: Model,
    pub api_key: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
}

impl std::fmt::Debug for ResolvedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModel")
            .field("provider", &self.model.provider)
            .field("id", &self.model.id)
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .finish_non_exhaustive()
    }
}

/// `provider/id` of a model, lowercased for selector comparison.
fn selector_of(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id).to_lowercase()
}

fn registry(agent_dir: &Path, cwd: &Path) -> pa_core::models::ModelRegistry {
    let mut registry = pa_core::models::ModelRegistry::for_session(agent_dir, cwd);
    registry.load_private_authorization_from_cache();
    registry
}

fn with_auth(
    registry: &mut pa_core::models::ModelRegistry,
    model: &Model,
) -> Result<ResolvedModel, String> {
    let auth = registry.get_api_key_and_headers(model, model.headers.as_ref());
    if !auth.ok {
        return Err(auth
            .error
            .unwrap_or_else(|| format!("no credential for provider {}", model.provider)));
    }
    Ok(ResolvedModel {
        model: model.clone(),
        api_key: auth.api_key,
        headers: auth.headers,
    })
}

/// Resolve a `provider/id` selector against the registry, honouring the
/// global `allowedModels` pin. Reads files: call it off the async runtime.
///
/// # Errors
///
/// A message naming why the selector cannot serve (unknown, outside the
/// pin, no credential).
pub fn resolve_selector(
    agent_dir: &Path,
    cwd: &Path,
    selector: &str,
    allowed_models: Option<&[String]>,
) -> Result<ResolvedModel, String> {
    let wanted = selector.trim().to_lowercase();
    if let Some(allowlist) = allowed_models {
        if !pa_core::models::model_allowed(&wanted, allowlist) {
            return Err(format!("model {selector} is outside the allowedModels pin"));
        }
    }
    let mut registry = registry(agent_dir, cwd);
    let model = registry
        .get_all()
        .iter()
        .find(|model| selector_of(model) == wanted)
        .cloned()
        .ok_or_else(|| format!("model {selector} is not registered"))?;
    with_auth(&mut registry, &model)
}

/// The model that runs Anthropic's server-side `web_search`: the
/// configured `searchModel`; else the session's own model when it is an
/// Anthropic Messages model of the first-party provider; else the cheapest
/// tier (haiku, then sonnet, then any) of the authenticated first-party
/// Anthropic models. `None` when no Anthropic login or key exists.
///
/// # Errors
///
/// A configured `searchModel` that cannot serve, or one that is not an
/// Anthropic Messages model.
pub fn resolve_search_model(
    agent_dir: &Path,
    cwd: &Path,
    configured: Option<&str>,
    allowed_models: Option<&[String]>,
    session_model: (&str, &str),
) -> Result<Option<ResolvedModel>, String> {
    if let Some(selector) = configured {
        let resolved = resolve_selector(agent_dir, cwd, selector, allowed_models)?;
        if resolved.model.api != ANTHROPIC_MESSAGES_API {
            return Err(format!(
                "web.searchModel {selector} is not an Anthropic Messages model (api {})",
                resolved.model.api
            ));
        }
        return Ok(Some(resolved));
    }
    let allowed = |model: &Model| {
        allowed_models
            .is_none_or(|allowlist| pa_core::models::model_allowed(&selector_of(model), allowlist))
    };
    let first_party =
        |model: &Model| model.provider == ANTHROPIC_PROVIDER && model.api == ANTHROPIC_MESSAGES_API;
    let mut registry = registry(agent_dir, cwd);
    let (session_provider, session_id) = session_model;
    let session = registry
        .get_all()
        .iter()
        .find(|model| model.provider == session_provider && model.id == session_id)
        .filter(|model| first_party(model) && allowed(model))
        .cloned();
    if let Some(model) = session {
        if let Ok(resolved) = with_auth(&mut registry, &model) {
            return Ok(Some(resolved));
        }
    }
    let candidates: Vec<Model> = registry
        .get_available()
        .into_iter()
        .filter(|model| first_party(model) && allowed(model))
        .cloned()
        .collect();
    let tier = |model: &Model| {
        let id = model.id.to_lowercase();
        if id.contains("haiku") {
            0
        } else if id.contains("sonnet") {
            1
        } else {
            2
        }
    };
    let Some(model) = candidates.iter().min_by_key(|model| tier(model)).cloned() else {
        return Ok(None);
    };
    Ok(with_auth(&mut registry, &model).ok())
}

/// What one completion produced.
#[derive(Debug, Clone)]
pub struct Completion {
    /// The concatenated text blocks.
    pub text: String,
    pub message: AssistantMessage,
}

/// One completion hooks: a payload rewrite and a raw event observer.
#[derive(Clone, Default)]
pub struct CompletionHooks {
    pub on_payload: Option<OnPayloadHook>,
    pub on_event: Option<OnEventHook>,
}

/// Run one user prompt against `resolved` and return its text. Thinking
/// stays at the provider default off; the call is cancelled with `cancel`.
///
/// # Errors
///
/// The provider's error, or the error message of a reply that stopped on
/// an error.
pub async fn complete(
    resolved: &ResolvedModel,
    system_prompt: Option<String>,
    prompt: String,
    max_tokens: Option<u64>,
    hooks: CompletionHooks,
    cancel: CancellationToken,
) -> Result<Completion, String> {
    let context = Context {
        system_prompt,
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text(prompt),
            timestamp: 0,
            rest: serde_json::Map::default(),
        })],
        tools: None,
    };
    let options = SimpleStreamOptions::from_base(StreamOptions {
        api_key: resolved.api_key.clone(),
        headers: resolved
            .headers
            .clone()
            .map(|headers| headers.into_iter().collect()),
        signal: Some(cancel.clone()),
        max_tokens,
        on_payload: hooks.on_payload,
        on_event: hooks.on_event,
        ..Default::default()
    });
    let reply = tokio::select! {
        () = cancel.cancelled() => return Err("cancelled".to_string()),
        reply = pa_ai::complete_simple(&resolved.model, &context, Some(options)) => reply,
    };
    let message = reply.map_err(|error| error.to_string())?;
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(message
            .error_message
            .unwrap_or_else(|| "the model call failed".to_string()));
    }
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    Ok(Completion { text, message })
}
