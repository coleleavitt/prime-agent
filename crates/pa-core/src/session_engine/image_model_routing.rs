//! The image-model routing host seam: turn dispatch consults this seam when the delivered
//! batch attaches image blocks. `None` on the session keeps image turns on the session model
//! (verification harnesses, and the daemon worker whose turn dispatch owns routing itself).

use std::sync::Arc;

use pa_agent::stream::{LlmContext, StreamFn, StreamRequestOptions};
use pa_agent::types::Model as AgentModel;
use pa_types::sync::{MutexExt, RwLockExt};

use crate::models::ResolvedImageModel;
use crate::session_engine::provider_adapter::{ProviderTarget, stream_once};

/// The routing decision for one dispatched batch: `Ok(None)` when the batch does not
/// route, `Err` the actionable refusal that fails the turn. The host owns the session-model
/// and per-request-field reads (the agent-state model descriptor is lossy - no input modalities).
pub type ImageRouteDecisionFn = Arc<
    dyn Fn(bool, pa_types::ai::ModelThinkingLevel) -> Result<Option<ResolvedImageModel>, String>
        + Send
        + Sync,
>;

/// Swap the host's serving target to the routed image model, or restore the session target;
/// called with the fresh decision of every admitted batch, so a stale route never outlives
/// the next dispatch.
pub type ImageRouteTargetSwapFn = Arc<dyn Fn(Option<&ResolvedImageModel>) + Send + Sync>;

/// The host seam for image-model routing.
#[derive(Clone)]
pub struct ImageModelRouter {
    pub decide: ImageRouteDecisionFn,
    pub swap_target: ImageRouteTargetSwapFn,
}

impl std::fmt::Debug for ImageModelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageModelRouter").finish_non_exhaustive()
    }
}

/// The armed image route stays AUTHORITATIVE while an episode is armed: a
/// `set_model` picker write to the slot lands only when the settle clears it.
///
/// # Panics
///
/// Panics at stream time if neither an armed route nor the slot holds a
/// target (the slot is set before the first stream).
#[must_use]
pub fn route_authoritative_stream_fn(
    provider_target: Arc<std::sync::RwLock<Option<ProviderTarget>>>,
    armed_target: std::sync::Arc<std::sync::Mutex<Option<ProviderTarget>>>,
) -> StreamFn {
    Arc::new(
        move |_requested: AgentModel, context: LlmContext, options: StreamRequestOptions| {
            let armed = armed_target.lock_or_recover().clone();
            let target = armed
                .or_else(|| provider_target.read_or_recover().clone())
                .expect("provider target set before the first stream");
            let ProviderTarget {
                api_key,
                model,
                service_tier,
                headers,
            } = target;
            Box::pin(async move {
                stream_once(&model, api_key, service_tier, headers, context, options)
            })
        },
    )
}

/// The image router over a switchable provider-target slot (the print and
/// RPC roots): the routing decision for one dispatched batch (`Err` fails
/// the turn with the actionable refusal) and the serving-target swap
/// (`None` restores the target).
#[must_use]
pub fn slot_image_model_router(
    provider_target: &std::sync::Arc<std::sync::RwLock<Option<ProviderTarget>>>,
    armed_target: std::sync::Arc<std::sync::Mutex<Option<ProviderTarget>>>,
    cwd: std::path::PathBuf,
    agent_dir: std::path::PathBuf,
    session_model: pa_types::ai::Model,
) -> ImageModelRouter {
    // The pre-route session target, captured at the FIRST arm (not at build):
    // the restore must return the SWITCHED-TO target, never the build-time one.
    let armed_from = Arc::new(std::sync::Mutex::new(None));
    // Holds the routed target the arm wrote, so the settle can tell a slot
    // that still holds the route from one a mid-run `/model` switch rewrote.
    let armed_to = armed_target;
    let decide_agent_dir = agent_dir.clone();
    let swap_cwd = cwd.clone();
    let decide_provider_target = Arc::clone(provider_target);
    let decide_armed_from = Arc::clone(&armed_from);
    let decide = Arc::new(
        move |carries_images: bool,
              thinking_level: pa_types::ai::ModelThinkingLevel|
              -> Result<Option<crate::models::ResolvedImageModel>, String> {
            if !carries_images {
                return Ok(None);
            }
            // The routing decision needs the SESSION model: during an armed episode
            // the live slot holds the ROUTED target; un-armed, it is the live slot.
            let armed_capture = decide_armed_from
                .lock_or_recover()
                .as_ref()
                .map(|target: &ProviderTarget| target.model.clone());
            let session_model = armed_capture
                .or_else(|| {
                    decide_provider_target
                        .read_or_recover()
                        .as_ref()
                        .map(|target| target.model.clone())
                })
                .unwrap_or_else(|| session_model.clone());
            let settings = crate::settings::SettingsManager::create(&cwd, &decide_agent_dir);
            let image_model_reference = settings.get_image_model();
            let block_images = settings.get_block_images();
            let mut registry = crate::models::ModelRegistry::for_session(&decide_agent_dir, &cwd);
            registry.load_private_authorization_from_cache();
            let available: Vec<pa_types::ai::Model> =
                registry.get_available().into_iter().cloned().collect();
            // Route acceptance uses the same resolved-auth result the arm path
            // installs: a provider can be signed in while its key resolution still fails.
            let resolvable_auth: std::collections::HashSet<(String, String)> = available
                .iter()
                .filter(|model| {
                    registry
                        .get_api_key_and_headers(model, model.headers.as_ref())
                        .ok
                })
                .map(|model| (model.provider.clone(), model.id.clone()))
                .collect();
            crate::models::resolve_image_model_override(&crate::models::ImageModelRoutingInputs {
                session_model: &session_model,
                thinking_level,
                service_tier: None,
                image_model_reference: image_model_reference.as_deref(),
                available_models: &available,
                // Keyed (provider, id): one provider's authenticated row must not vouch for
                // another provider's same-id model.
                has_configured_auth: &|model| {
                    resolvable_auth.contains(&(model.provider.clone(), model.id.clone()))
                },
                block_images,
            })
        },
    );
    let swap_target = {
        let provider_target = Arc::clone(provider_target);
        let armed_to = Arc::clone(&armed_to);
        Arc::new(move |route: Option<&crate::models::ResolvedImageModel>| {
            if let Some(resolved) = route {
                // The first swap of the episode captures the session target it replaces.
                let mut armed_from = armed_from.lock_or_recover();
                if armed_from.is_none() {
                    armed_from.clone_from(&provider_target.read_or_recover());
                }
                let mut registry = crate::models::ModelRegistry::for_session(&agent_dir, &swap_cwd);
                registry.load_private_authorization_from_cache();
                let resolved_auth = registry
                    .get_api_key_and_headers(&resolved.model, resolved.model.headers.as_ref());
                let target = ProviderTarget {
                    api_key: resolved_auth.api_key,
                    headers: resolved_auth.headers,
                    model: resolved.model.clone(),
                    service_tier: resolved.service_tier,
                };
                *armed_to.lock_or_recover() = Some(target.clone());
                *provider_target.write_or_recover() = Some(target);
            } else {
                // Restore the captured session target ONLY when the slot still holds the routed
                // target the arm wrote.
                let captured = armed_from.lock_or_recover().take();
                let routed = armed_to.lock_or_recover().take();
                let current = provider_target.read_or_recover().clone();
                // The full serving target, credentials included: a switch may keep the same
                // id while rotating its api key; the guard treats that slot as switched.
                let still_routed = match (&current, &routed) {
                    (Some(current), Some(routed)) => {
                        current.model.id == routed.model.id
                            && current.service_tier == routed.service_tier
                            && current.api_key == routed.api_key
                            && current.headers == routed.headers
                    }
                    _ => true,
                };
                if still_routed && let Some(target) = captured.or(current) {
                    *provider_target.write_or_recover() = Some(target);
                }
            }
        })
    };
    ImageModelRouter {
        decide,
        swap_target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settle restores the captured session target only while the slot still holds the route;
    /// a mid-run `/model` switch rewrote the slot, and stays.
    #[test]
    fn headless_image_router_settle_preserves_a_mid_run_model_switch() {
        fn fixture_model(id: &str) -> pa_types::ai::Model {
            pa_types::ai::Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "anthropic-messages".to_string(),
                provider: "anthropic".to_string(),
                base_url: "https://x".to_string(),
                reasoning: true,
                thinking_level_map: None,
                input: vec![
                    pa_types::ai::ModelInput::Text,
                    pa_types::ai::ModelInput::Image,
                ],
                cost: pa_types::ai::ModelCost {
                    input: 1.0.into(),
                    output: 2.0.into(),
                    cache_read: 0.0.into(),
                    cache_write: 0.0.into(),
                },
                context_window: 200_000,
                max_tokens: 8192,
                max_tokens_explicit: false,
                featured: None,
                serving: None,
                headers: None,
                compat: None,
            }
        }
        fn target(model: pa_types::ai::Model) -> ProviderTarget {
            ProviderTarget {
                api_key: None,
                headers: None,
                model,
                service_tier: None,
            }
        }
        let home = tempfile::TempDir::new().unwrap();
        let agent_dir = home.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let session_model = fixture_model("session-model");
        let provider_target = Arc::new(std::sync::RwLock::new(Some(target(session_model.clone()))));
        let armed_target: std::sync::Arc<std::sync::Mutex<Option<ProviderTarget>>> =
            Arc::new(std::sync::Mutex::new(None));
        let router = super::slot_image_model_router(
            &provider_target,
            Arc::clone(&armed_target),
            home.path().to_path_buf(),
            agent_dir,
            session_model,
        );
        let expected_route = crate::models::ResolvedImageModel {
            model: fixture_model("image-model"),
            thinking_level: pa_types::ai::ModelThinkingLevel::High,
            service_tier: None,
        };
        (router.swap_target)(Some(&expected_route));
        assert_eq!(
            provider_target.read().unwrap().as_ref().unwrap().model.id,
            "image-model"
        );
        let switched_to = target(fixture_model("switched-model"));
        *provider_target.write().unwrap() = Some(switched_to);
        (router.swap_target)(None);
        assert_eq!(
            provider_target.read().unwrap().as_ref().unwrap().model.id,
            "switched-model"
        );
        // The next episode captures the live slot at ITS first arm: its baseline is the post-switch
        // session model.
        (router.swap_target)(Some(&expected_route));
        (router.swap_target)(None);
        assert_eq!(
            provider_target.read().unwrap().as_ref().unwrap().model.id,
            "switched-model"
        );
    }
}
