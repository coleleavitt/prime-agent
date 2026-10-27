//! Read-only feasibility probe for the Claude Code memory service.
//!
//! Makes ONE read-only request — `GET /v1/code/local/memory/mounts` — to
//! `api.anthropic.com`, authorized with the shared Anthropic OAuth bearer
//! from `~/.anthropic-accounts`. It answers one question: does the server
//! accept prime-agent's token for the memory API, or is it gated to the
//! Claude Code client? It performs NO writes and prints only the token's
//! length (never the token). Not part of the test suite; run it by hand:
//!
//!   cargo +1.98.1 run -p pa-anthropic-auth --example `memory_mounts_probe`
use std::time::Duration;

#[tokio::main]
async fn main() {
    let source = pa_anthropic_auth::shared_source();
    let token = match source.bearer() {
        Ok(token) => token,
        Err(error) => {
            eprintln!("could not resolve a bearer: {error}");
            std::process::exit(2);
        }
    };
    eprintln!("resolved a bearer ({} chars)", token.len());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client");
    let url = "https://api.anthropic.com/v1/code/local/memory/mounts";
    let response = client
        .get(url)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("anthropic-version", "2023-06-01")
        .header("x-app", "cli")
        .header(
            "anthropic-beta",
            "agent-memory-2026,context-management-2025-06-27",
        )
        .header("user-agent", "pa-memory-probe (prime-agent, read-only)")
        .send()
        .await;
    match response {
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            println!("HTTP {status}");
            println!("{body}");
        }
        Err(error) => {
            eprintln!("request failed: {error}");
            std::process::exit(1);
        }
    }
}
