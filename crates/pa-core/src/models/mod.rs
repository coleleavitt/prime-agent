//! Model subsystem: resolver and Prime Inference private models.

pub use allowlist::{ModelAllowlistRefusal, model_allowed};
pub use catalog_chain::{
    catalog_for,
    install_catalog,
    prime_credentials_for_dir,
    spawn_hourly_refresh,
    startup_refresh,
};
/// The refresh triggers (`pa-models`' own enum — the daemon's picker-open
/// and auth-change wiring selects it; pa-daemon's runtime surface goes
/// through pa-core, so the trigger rides this re-export).
pub use pa_models::RefreshTrigger;
pub use private_auth::{
    PRIVATE_PRIME_AUTHORIZATION_CACHE_TTL_MS,
    PrivatePrimeAuthorizationCache,
    get_private_prime_inference_models,
    private_prime_authorization_fingerprint,
    read_private_prime_authorization_cache,
    write_private_prime_authorization_cache,
};
pub use registry::{
    ModelRegistry,
    ProviderRequestConfig,
    ResolvedRequestAuth,
    SetModelSelectionError,
};

pub mod allowlist;
pub(crate) mod catalog_chain;
pub(crate) mod custom;
pub mod image_model_routing;
pub(crate) mod prime_inference;
pub(crate) mod prime_inference_catalog;
pub(crate) mod private_auth;
pub(crate) mod registry;
pub(crate) mod resolver;
pub(crate) mod session_restore;

pub use custom::{
    CustomModelsResult,
    ModelOverride,
    ModelsConfig,
    ProviderOverride,
    apply_model_override,
    load_custom_models,
    merge_compat,
    parse_models_config,
    strip_json_comments,
    validate_config,
};
pub use image_model_routing::{
    ImageModelRoutingInputs,
    ResolvedImageModel,
    resolve_image_model_override,
};
pub use prime_inference::{
    PRIME_INFERENCE_BASE_URL,
    is_private_prime_inference_model,
    is_private_prime_inference_model_id,
    private_prime_inference_models,
};
pub use prime_inference_catalog::{
    PrimeInferenceCatalogEntry,
    build_prime_inference_models,
    parse_prime_inference_model_catalog,
};
pub use resolver::{
    InitialModelOptions,
    PRIME_INFERENCE_DEFAULT_MODEL_ID,
    ResolveCliModelResult,
    ScopedModel,
    build_fallback_model,
    failover_candidates,
    find_exact_model_reference_match,
    find_initial_model,
    find_preferred_default_model,
    resolve_cli_model,
    resolve_fallback_models,
    resolve_model_scope_from_models,
};
pub use session_restore::{
    SESSION_MODEL_RESTORE_READINESS_TIMEOUT_MS,
    find_session_model_with_readiness_wait,
};
