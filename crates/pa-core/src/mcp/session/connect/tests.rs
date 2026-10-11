//! Credential, environment and launch resolution: ports of the in-kernel
//! client's `_headers` / `_auth_identity` / `_stdio_env` tests against the
//! host auth store.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::auth::{AuthStorage, AuthStorageData};
use crate::mcp::session::error::McpErrorKind;

fn env(pairs: &[(&str, &str)]) -> KernelEnv {
    KernelEnv {
        environment: KernelEnvironment::Inherit,
        overrides: pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect(),
        host: false,
    }
}

fn credentials(entries: Value) -> McpCredentials {
    let Value::Object(map) = entries else {
        panic!("auth entries must be an object");
    };
    McpCredentials::new(
        Arc::new(tokio::sync::Mutex::new(AuthStorage::in_memory_without_env(
            &AuthStorageData(map),
            Arc::new(crate::mcp::McpOAuth::new()),
        ))),
        None,
    )
}

fn far_future_ms() -> i64 {
    now_ms() + 3_600_000
}

fn unavailable(server: &str) -> McpSessionError {
    McpSessionError::credentials_unavailable(server)
}

async fn headers(
    server: &str,
    config: &Value,
    store: &McpCredentials,
) -> Result<Vec<(String, String)>, McpSessionError> {
    http_headers(server, config, &env(&[]), store).await
}

#[test]
fn the_stdio_env_is_the_safe_set_plus_resolved_references() {
    let config = json!({ "command": "x", "env": { "TOKEN": { "env": "SECRET" } } });
    let launch = stdio_launch(
        "svc",
        &config,
        &env(&[("PATH", "/bin"), ("SECRET", "value"), ("UNRELATED", "no")]),
        Path::new("/work"),
    )
    .unwrap();
    assert_eq!(
        launch,
        StdioLaunch {
            command: "x".to_string(),
            args: Vec::new(),
            cwd: "/work".into(),
            env: vec![
                ("PATH".to_string(), "/bin".to_string()),
                ("TOKEN".to_string(), "value".to_string()),
            ],
            secrets: vec!["value".to_string()],
            disclosable: true,
            private_values: vec![
                "SECRET".to_string(),
                "/work".to_string(),
                "TOKEN".to_string(),
                "env".to_string(),
                "x".to_string(),
            ],
        }
    );
}

#[test]
fn an_acp_stdio_env_is_literal_and_ambient_free() {
    let config =
        json!({ "command": "x", "credentialSource": "acp", "env": { "TOKEN": "task-secret" } });
    let launch = stdio_launch(
        "svc",
        &config,
        &env(&[("PATH", "/bin"), ("UNRELATED", "ambient-secret")]),
        Path::new("/work"),
    )
    .unwrap();
    assert_eq!(
        launch.env,
        vec![
            ("PATH".to_string(), "/bin".to_string()),
            ("TOKEN".to_string(), "task-secret".to_string()),
        ]
    );
}

#[test]
fn a_short_configured_value_makes_the_launch_undisclosable() {
    let config = json!({ "command": "x", "credentialSource": "acp", "env": { "K": "xy" } });
    let launch = stdio_launch("svc", &config, &env(&[]), Path::new("/w")).unwrap();
    assert_eq!(
        (launch.disclosable, launch.secrets),
        (false, Vec::<String>::new())
    );
}

#[test]
fn the_scrub_policy_hides_provider_keys_from_references() {
    let scrubbed = KernelEnv {
        environment: KernelEnvironment::ScrubCredentials,
        overrides: HashMap::new(),
        host: true,
    };
    let config = json!({ "command": "x", "env": { "KEY": { "env": "OPENAI_API_KEY" } } });
    // Whatever the host holds, the scrubbed kernel never sees it.
    assert_eq!(
        stdio_launch("svc", &config, &scrubbed, Path::new("/w")),
        Err(McpSessionError::value(
            "MCP stdio environment reference for 'KEY' is unavailable"
        ))
    );
    // The daemon worker identity never reaches a server either.
    assert_eq!(scrubbed.get("PRIME_AGENT_INTERNAL_DAEMON_TOKEN"), None);
}

#[test]
fn stored_values_resolve_like_the_host() {
    let env = env(&[("MY_MCP_KEY", "resolved-secret"), ("EMPTY", "")]);
    assert_eq!(
        [
            resolve_config_value("MY_MCP_KEY", &env),
            resolve_config_value("key-abc", &env),
            resolve_config_value("!security find-key", &env),
            resolve_config_value("  ", &env),
            resolve_config_value("EMPTY", &env),
        ],
        [
            "resolved-secret".to_string(),
            "key-abc".to_string(),
            String::new(),
            String::new(),
            "EMPTY".to_string(),
        ]
    );
}

#[tokio::test]
async fn acp_headers_never_consult_or_override_host_oauth() {
    let store = credentials(json!({
        "mcp:linear": { "type": "oauth", "access": "host-token", "expires": far_future_ms(), "endpoint": "https://task.example/mcp" }
    }));
    let config = json!({
        "type": "http",
        "url": "https://task.example/mcp",
        "headers": { "Authorization": "Bearer task-token" },
        "credentialSource": "acp",
        "oauth": true,
    });
    assert_eq!(
        headers("linear", &config, &store).await,
        Ok(vec![(
            "Authorization".to_string(),
            "Bearer task-token".to_string()
        )])
    );
}

#[tokio::test]
async fn an_endpoint_bound_credential_never_attaches_to_another_url() {
    let store = credentials(json!({
        "mcp:remote": { "type": "oauth", "access": "old-token", "expires": far_future_ms(), "endpoint": "https://old.example/mcp" }
    }));
    let at = |url: &str| json!({ "type": "http", "oauth": true, "url": url });
    assert_eq!(
        (
            headers("remote", &at("https://new.example/mcp"), &store).await,
            // Exact match only: a trailing slash is a changed entry.
            headers("remote", &at("https://old.example/mcp/"), &store).await,
            headers("remote", &at("https://old.example/mcp"), &store).await,
        ),
        (
            Err(unavailable("remote")),
            Err(unavailable("remote")),
            Ok(vec![(
                "Authorization".to_string(),
                "Bearer old-token".to_string()
            )]),
        )
    );
}

#[tokio::test]
async fn an_unbound_credential_requires_relogin() {
    let store = credentials(json!({
        "mcp:remote": { "type": "oauth", "access": "unbound-token", "expires": far_future_ms() }
    }));
    let config = json!({ "type": "http", "oauth": true, "url": "https://srv.example/mcp" });
    assert_eq!(
        headers("remote", &config, &store).await,
        Err(unavailable("remote"))
    );
}

#[tokio::test]
async fn static_token_headers_attach_only_the_bound_bearer() {
    let config = json!({ "type": "http", "url": "https://api.example/mcp", "credentialSource": "static-token" });
    let bound = json!({
        "type": "mcp_static_token",
        "endpoint": "https://api.example/mcp",
        "bearer": "pasted-token",
    });
    let mut elsewhere = bound.clone();
    elsewhere["endpoint"] = json!("https://old.example/mcp");
    let results = [
        headers("github", &config, &credentials(json!({ "mcp:github": bound }))).await,
        headers("github", &config, &credentials(json!({ "mcp:github": elsewhere }))).await,
        // A non-static shape or no credential fails closed, never anonymous.
        headers(
            "github",
            &config,
            &credentials(json!({ "mcp:github": { "type": "oauth", "access": "x", "expires": far_future_ms(), "endpoint": "https://api.example/mcp" } })),
        )
        .await,
        headers("github", &config, &credentials(json!({}))).await,
    ];
    assert_eq!(
        results,
        [
            Ok(vec![(
                "Authorization".to_string(),
                "Bearer pasted-token".to_string()
            )]),
            Err(unavailable("github")),
            Err(unavailable("github")),
            Err(unavailable("github")),
        ]
    );
    assert_eq!(
        unavailable("github"),
        McpSessionError::new(
            McpErrorKind::CredentialsUnavailable,
            "MCP credentials for 'github' are not available. Ask the user to connect it \
             (/plugins or /mcp login github); do not ask them to set environment variables."
        )
    );
}

#[tokio::test]
async fn a_static_bearer_is_never_env_or_command_resolved() {
    let config = json!({ "type": "http", "url": "https://api.example/mcp", "credentialSource": "static-token" });
    let env = env(&[("GITHUB_PAT_TOKEN", "env-resolved-token")]);
    for pasted in ["GITHUB_PAT_TOKEN", "!sh -c secret", "  spaced-token  "] {
        let store = credentials(json!({
            "mcp:github": { "type": "mcp_static_token", "endpoint": "https://api.example/mcp", "bearer": pasted }
        }));
        let expected = pasted.trim();
        assert_eq!(
            (
                http_headers("github", &config, &env, &store).await,
                auth_identity("github", &config, &env, &store).await,
            ),
            (
                Ok(vec![(
                    "Authorization".to_string(),
                    format!("Bearer {expected}")
                )]),
                Ok(sha256_hex(expected)),
            )
        );
    }
    assert_eq!(
        auth_identity("github", &config, &env, &credentials(json!({}))).await,
        Err(unavailable("github"))
    );
}

#[tokio::test]
async fn identities_hash_the_token_or_name_an_anonymous_connection() {
    let store = credentials(json!({}));
    let anonymous = json!({ "type": "http", "url": "https://open.example/mcp" });
    let env_token = json!({ "type": "http", "url": "https://open.example/mcp", "bearerTokenEnvVar": "SVC_TOKEN" });
    assert_eq!(
        (
            auth_identity("svc", &anonymous, &env(&[]), &store).await,
            auth_identity("svc", &env_token, &env(&[("SVC_TOKEN", " tok ")]), &store).await,
            auth_identity("svc", &env_token, &env(&[]), &store).await,
        ),
        (
            Ok("anonymous".to_string()),
            Ok(sha256_hex("tok")),
            Err(unavailable("svc")),
        )
    );
}

#[tokio::test]
async fn an_expiring_login_that_cannot_refresh_fails_with_a_refresh_error() {
    let store = credentials(json!({
        "mcp:remote": { "type": "oauth", "access": "stale", "expires": 1, "endpoint": "https://srv.example/mcp" }
    }));
    let config = json!({ "type": "http", "oauth": true, "url": "https://srv.example/mcp" });
    assert_eq!(
        auth_identity("remote", &config, &env(&[]), &store).await,
        Err(McpSessionError::runtime(
            "Could not refresh MCP credentials for 'remote'"
        ))
    );
}
