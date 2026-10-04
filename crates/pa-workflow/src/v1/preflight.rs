//! Model and credential preflight (TS `preflightWorkflowModel`): before any
//! provider I/O the selector must resolve, through the same resolution RLM
//! children use, to a model the registry carries, and that model's provider
//! must hold a usable credential — an API key, a configured
//! credential-bearing header, or an explicit `authHeader: false` no-auth
//! policy. Anything else fails closed as `model_resolution_failed`.

use std::collections::BTreeMap;

use pa_core::auth::AuthSource;
use pa_core::models::ModelRegistry;
use pa_core::session_engine::rlm_in_process::resolve_child_model;
use pa_types::ai::Model as AiModel;

/// Header names that carry a credential (case-insensitive). Arbitrary
/// metadata headers (`User-Agent`, routing tags) never authenticate.
const CREDENTIAL_HEADER_NAMES: [&str; 5] = [
    "authorization",
    "proxy-authorization",
    "api-key",
    "x-api-key",
    "x-auth-token",
];

/// A model cleared for one provider turn, with its request credential.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cleared {
    pub model: AiModel,
    pub api_key: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
}

impl Cleared {
    pub(crate) fn selector(&self) -> String {
        format!("{}/{}", self.model.provider, self.model.id)
    }
}

/// Resolve `reference` (`None`: the session model) and clear it.
///
/// # Errors
///
/// Returns the refusal message when the selector does not resolve, the
/// model is not registered, its provider's credential is stale or expired,
/// or no usable credential or explicit no-auth policy exists.
pub(crate) fn preflight(
    registry: &mut ModelRegistry,
    reference: Option<&str>,
    session_model: &pa_agent::types::Model,
) -> Result<Cleared, String> {
    let resolved = resolve_child_model(registry, reference, Some(session_model), "workflow agent")
        .map_err(|error| format!("{error:#}"))?;
    let (provider, id) = (&resolved.model.provider, &resolved.model.id);
    let model = registry
        .get_all()
        .iter()
        .find(|model| &model.provider == provider && &model.id == id)
        .cloned()
        .ok_or_else(|| format!("Model \"{provider}/{id}\" is not registered"))?;
    let status = registry.get_provider_auth_status(provider);
    if status.source == Some(AuthSource::Stale) || status.label.as_deref() == Some("expired") {
        return Err(format!(
            "Provider \"{provider}\" credentials are stale or expired"
        ));
    }
    let auth = registry.get_api_key_and_headers(&model, None);
    if !auth.ok {
        return Err(auth
            .error
            .unwrap_or_else(|| format!("No credential found for \"{provider}\"")));
    }
    let config = registry.provider_request_config(provider);
    let explicit_no_auth = config.and_then(|config| config.auth_header) == Some(false);
    let credential_header = config
        .and_then(|config| config.headers.as_ref())
        .is_some_and(|configured| {
            configured.keys().any(|name| {
                CREDENTIAL_HEADER_NAMES.contains(&name.to_ascii_lowercase().as_str())
                    && auth.headers.as_ref().is_some_and(|resolved| {
                        resolved.iter().any(|(resolved_name, value)| {
                            resolved_name.eq_ignore_ascii_case(name) && !value.trim().is_empty()
                        })
                    })
            })
        });
    if auth.api_key.is_none() && !credential_header && !explicit_no_auth {
        return Err(format!(
            "Provider \"{provider}\" has neither usable credentials nor an explicit no-auth policy"
        ));
    }
    Ok(Cleared {
        model,
        api_key: auth.api_key,
        headers: auth.headers,
    })
}
