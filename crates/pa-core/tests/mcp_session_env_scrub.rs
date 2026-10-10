// Pedantic-gate dispositions as src/lib.rs (large_futures).
#![allow(clippy::large_futures)]
// Spawns a POSIX stdio server process.
#![cfg(unix)]

//! Verifier: the `kernel.environment` policy reaches the host-owned MCP
//! stdio servers. Under `inherit` a server's `{"env": NAME}` reference sees
//! the host variable the kernel would; under `scrub-credentials` the
//! model-provider keys are as absent for it as for the kernel. Its own test
//! binary: it sets a provider key in the process environment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_core::kernel::shared::{
    with_host_request_cancellation, HostRequestHandlers, HostRequestPayload, KernelEnvironment,
};
use pa_core::mcp::{
    EnvRef, McpManager, McpManagerOptions, McpOAuth, McpServerConfig, McpSessionOptions,
};
use serde_json::{json, Value};

fn python() -> Option<PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

async fn fixture_env(python: &Path, environment: KernelEnvironment) -> Value {
    let dir = tempfile::TempDir::new().unwrap();
    let servers = HashMap::from([(
        "svc".to_string(),
        McpServerConfig::Stdio {
            command: python.to_string_lossy().to_string(),
            args: Some(vec![Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/mcp/stdio_server.py")
                .to_string_lossy()
                .to_string()]),
            cwd: None,
            env: Some(HashMap::from([(
                "FIXTURE_ENV".to_string(),
                EnvRef {
                    env: Some("OPENAI_API_KEY".to_string()),
                },
            )])),
            enabled: None,
            enabled_tools: None,
            disabled_tools: None,
            startup_timeout_ms: None,
            call_timeout_ms: None,
        },
    )]);
    let manager = Arc::new(Mutex::new(McpManager::new(McpManagerOptions {
        auth_storage: pa_core::auth::AuthStorage::create_with_oauth(
            dir.path(),
            Arc::new(McpOAuth::new()),
        ),
        get_user_servers: Box::new(move || Some(servers.clone())),
        begin_login: None,
        agent_dir: None,
        get_catalog_sources: None,
        remote_source: None,
        probe_override: None,
    })));
    let mut handlers = HostRequestHandlers::new();
    let sessions = McpManager::register_session_handlers(
        &manager,
        &mut handlers,
        McpSessionOptions {
            cwd: dir.path().to_path_buf(),
            environment,
            ..McpSessionOptions::default()
        },
    );
    let handler = handlers.get("mcp.session.call_tool").unwrap().clone();
    let reply = with_host_request_cancellation(
        tokio_util::sync::CancellationToken::new(),
        handler(HostRequestPayload {
            data: json!({ "server": "svc", "tool": "fixture/raw.tool" }),
            cell_source_code: None,
        }),
    )
    .await
    .unwrap();
    sessions.close_all().await;
    match reply["value"].as_str() {
        Some(text) => serde_json::from_str::<Value>(text).unwrap()["env"].clone(),
        None => reply["error"].clone(),
    }
}

#[tokio::test]
async fn scrub_credentials_hides_provider_keys_from_stdio_servers() {
    let Some(python) = python() else {
        return;
    };
    std::env::set_var("OPENAI_API_KEY", "sk-test-openai");
    assert_eq!(
        (
            fixture_env(&python, KernelEnvironment::Inherit).await,
            fixture_env(&python, KernelEnvironment::ScrubCredentials).await,
        ),
        (
            json!("sk-test-openai"),
            json!({
                "type": "ValueError",
                "message": "MCP stdio environment reference for 'FIXTURE_ENV' is unavailable",
            }),
        )
    );
}
