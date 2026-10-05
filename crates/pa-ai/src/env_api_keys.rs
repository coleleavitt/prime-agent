//! Environment variable API key resolution.

use std::path::PathBuf;

/// Providers keyed by one API-key environment variable.
const SINGLE_KEY_PROVIDERS: &[(&str, &str)] = &[
    ("openai", "OPENAI_API_KEY"),
    ("azure-openai-responses", "AZURE_OPENAI_API_KEY"),
    ("prime-inference", "PRIME_API_KEY"),
    ("deepseek", "DEEPSEEK_API_KEY"),
    ("google", "GEMINI_API_KEY"),
    ("google-vertex", "GOOGLE_CLOUD_API_KEY"),
    ("groq", "GROQ_API_KEY"),
    ("cerebras", "CEREBRAS_API_KEY"),
    ("xai", "XAI_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
    ("vercel-ai-gateway", "AI_GATEWAY_API_KEY"),
    ("zai", "ZAI_API_KEY"),
    ("mistral", "MISTRAL_API_KEY"),
    ("minimax", "MINIMAX_API_KEY"),
    ("minimax-cn", "MINIMAX_CN_API_KEY"),
    ("moonshotai", "MOONSHOT_API_KEY"),
    ("moonshotai-cn", "MOONSHOT_API_KEY"),
    ("huggingface", "HF_TOKEN"),
    ("fireworks", "FIREWORKS_API_KEY"),
    ("opencode", "OPENCODE_API_KEY"),
    ("opencode-go", "OPENCODE_API_KEY"),
    ("kimi-coding", "KIMI_API_KEY"),
    ("cloudflare-workers-ai", "CLOUDFLARE_API_KEY"),
    ("cloudflare-ai-gateway", "CLOUDFLARE_API_KEY"),
    ("xiaomi", "XIAOMI_API_KEY"),
    ("xiaomi-token-plan-cn", "XIAOMI_TOKEN_PLAN_CN_API_KEY"),
    ("xiaomi-token-plan-ams", "XIAOMI_TOKEN_PLAN_AMS_API_KEY"),
    ("xiaomi-token-plan-sgp", "XIAOMI_TOKEN_PLAN_SGP_API_KEY"),
];
/// `github-copilot`'s own variable; it also falls back to the generic `GH_TOKEN`/`GITHUB_TOKEN`.
const COPILOT_KEY_ENV_VARS: [&str; 3] = ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];
/// `ANTHROPIC_OAUTH_TOKEN` takes precedence over `ANTHROPIC_API_KEY`.
const ANTHROPIC_KEY_ENV_VARS: [&str; 2] = ["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"];

#[must_use]
pub fn get_api_key_env_vars(provider: &str) -> Option<Vec<&'static str>> {
    match provider {
        "github-copilot" => Some(COPILOT_KEY_ENV_VARS.to_vec()),
        "anthropic" => Some(ANTHROPIC_KEY_ENV_VARS.to_vec()),
        other => SINGLE_KEY_PROVIDERS
            .iter()
            .find(|(provider, _)| *provider == other)
            .map(|(_, env_var)| vec![*env_var]),
    }
}

/// Every environment variable Prime Agent reads as a model-provider API key, deduplicated: the
/// credentials a kernel with the `scrub-credentials` environment policy does not inherit. The
/// generic GitHub tokens (`GH_TOKEN`, `GITHUB_TOKEN`) are excluded: `github-copilot` only falls
/// back to them, and tools such as `gh` legitimately use them.
#[must_use]
pub fn provider_api_key_env_vars() -> Vec<&'static str> {
    let mut vars: Vec<&'static str> = ANTHROPIC_KEY_ENV_VARS
        .iter()
        .chain(&COPILOT_KEY_ENV_VARS[..1])
        .copied()
        .chain(SINGLE_KEY_PROVIDERS.iter().map(|(_, env_var)| *env_var))
        .collect();
    vars.sort_unstable();
    vars.dedup();
    vars
}

/// Find configured env vars for a provider. Ambient credential sources (AWS profiles, Google ADC)
/// are intentionally excluded; see `get_env_api_key`.
pub fn find_env_keys(provider: &str) -> Option<Vec<String>> {
    let env_vars = get_api_key_env_vars(provider)?;
    let found: Vec<String> = env_vars
        .into_iter()
        .filter(|env_var| std::env::var_os(env_var).is_some_and(|v| !v.is_empty()))
        .map(std::string::ToString::to_string)
        .collect();
    if found.is_empty() {
        None
    } else {
        Some(found)
    }
}

/// Get an API key for a provider from known environment variables. Returns the sentinel
/// "<authenticated>" for ambient credential sources (Google Vertex ADC, Amazon Bedrock profiles).
#[must_use]
pub fn get_env_api_key(provider: &str) -> Option<String> {
    if let Some(keys) = find_env_keys(provider) {
        if let Some(first) = keys.first() {
            if let Ok(value) = std::env::var(first) {
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
    }

    if provider == "google-vertex" {
        let has_credentials = has_vertex_adc_credentials();
        let has_project = ["GOOGLE_CLOUD_PROJECT", "GCLOUD_PROJECT"]
            .iter()
            .any(|var| std::env::var(var).is_ok_and(|v| !v.is_empty()));
        let has_location = std::env::var("GOOGLE_CLOUD_LOCATION").is_ok_and(|v| !v.is_empty());
        if has_credentials && has_project && has_location {
            return Some("<authenticated>".to_string());
        }
    }

    if provider == "amazon-bedrock" {
        let env = |name: &str| std::env::var(name).is_ok_and(|v| !v.is_empty());
        if env("AWS_PROFILE")
            || (env("AWS_ACCESS_KEY_ID") && env("AWS_SECRET_ACCESS_KEY"))
            || env("AWS_BEARER_TOKEN_BEDROCK")
            || env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
            || env("AWS_CONTAINER_CREDENTIALS_FULL_URI")
            || env("AWS_WEB_IDENTITY_TOKEN_FILE")
        {
            return Some("<authenticated>".to_string());
        }
    }

    None
}

fn has_vertex_adc_credentials() -> bool {
    if let Ok(gac_path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        if !gac_path.is_empty() {
            return std::path::Path::new(&gac_path).exists();
        }
    }
    default_adc_path().exists()
}

fn default_adc_path() -> PathBuf {
    let home = pa_types::platform::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    home.join(".config")
        .join("gcloud")
        .join("application_default_credentials.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_provider_env_vars() {
        assert_eq!(
            get_api_key_env_vars("prime-inference").unwrap(),
            vec!["PRIME_API_KEY"]
        );
        assert_eq!(
            get_api_key_env_vars("anthropic").unwrap(),
            vec!["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"]
        );
        assert!(get_api_key_env_vars("unknown-provider").is_none());
    }
}
