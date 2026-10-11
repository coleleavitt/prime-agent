//! Auth subsystem: credential storage, resolution priority, stale-marking.

pub(crate) mod credential_source;
pub(crate) mod manager;
pub(crate) mod notices;
pub(crate) mod prime_directory;
pub(crate) mod prime_inference;
pub(crate) mod prime_inference_login;
pub(crate) mod prime_traces;
pub(crate) mod provider_oauth;
pub(crate) mod resolve_config_value;
pub(crate) mod storage;
pub(crate) mod types;

pub use credential_source::{
    CredentialSourceError,
    CredentialSourceStatus,
    ProviderCredentialSource,
    RemovedLogin,
    SourcedCredential,
    StoredLoginCustody,
    StoredOAuthLogin,
    credential_source,
    credential_source_providers,
    install_credential_source,
};
pub use manager::{
    AuthApiKeyResult,
    AuthStorage,
    NoOAuth,
    OAuthIntegration,
    OAuthRefreshError,
    oauth_refresh_failed_message,
};
pub use notices::{
    AuthNotice,
    AuthNoticeSink,
    clear_auth_notice,
    raise_auth_notice,
    register_auth_notice_sink,
};
pub use prime_directory::PrimeDirectorySelection;
pub use prime_inference::{
    DEFAULT_PRIME_API_BASE_URL,
    DEFAULT_PRIME_FRONTEND_URL,
    DEFAULT_REQUEST_TIMEOUT_MS,
    PrimeAccessError,
    PrimeAccessFailure,
    PrimeCliConfig,
    PrimeHttp,
    PrimeHttpResponse,
    PrimeInferenceAuthConfig,
    ReqwestPrimeHttp,
    check_prime_inference_access,
    default_prime_cli_config_path,
    fetch_prime_teams,
    read_prime_cli_config,
    resolve_prime_inference_auth_config,
};
pub use prime_inference_login::{
    PrimeInferenceLoginCallbacks,
    PrimeInferenceLoginOptions,
    PrimeInferenceLoginResult,
    PrimeInferenceLoginSource,
    login_prime_inference,
};
pub use prime_traces::{
    PRIME_AGENT_TRACES_PROVIDER_ID,
    PRIME_AGENT_TRACES_PROVIDER_NAME,
    PrimeAgentTracesCallbacks,
    PrimeAgentTracesLoginOptions,
    PrimeAgentTracesLoginSource,
    PrimeAuthInfo,
    check_prime_agent_traces_access,
    login_prime_agent_traces,
    resolve_prime_agent_traces_base_url,
};
pub use provider_oauth::{
    ANTHROPIC_PROVIDER_ID,
    GITHUB_COPILOT_PROVIDER_ID,
    OPENAI_CODEX_PROVIDER_ID,
    ProviderOAuth,
    XAI_PROVIDER_ID,
};
pub use storage::{
    AuthStorageBackend,
    FileAuthStorageBackend,
    InMemoryAuthStorageBackend,
    UnsavedRefreshKept,
    parse_storage_data,
};
pub use types::{
    AuthCredential,
    AuthSource,
    AuthSourceToken,
    AuthStatus,
    AuthStorageData,
    PRIME_INFERENCE_PROVIDER_ID,
    PrimeTeamAssignment,
    PrimeTeamCredential,
    SERPER_CREDENTIAL_ID,
    SERPER_CREDENTIAL_NAME,
    StoredPrimeTeam,
};
