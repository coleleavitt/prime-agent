//! Test fixtures shared by the crate's test modules: a temporary store, a
//! loopback token endpoint, a mock Messages endpoint. Never the user's
//! store, Claude Code's files, or the network.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anthropic::token::{AccessToken, Credential, OAuthTokens, RefreshToken};
use anthropic::{Account, AccountStore};
use chrono::{Duration, Utc};
use pa_types::sync::MutexExt;

use crate::{SharedStoreConfig, SharedStoreSource};

pub(crate) const ROTATED_ACCESS: &str = "sk-ant-oat01-rotated-rotated-rotated-00";
pub(crate) const ROTATED: &str = r#"{"access_token":"sk-ant-oat01-rotated-rotated-rotated-00","refresh_token":"sk-ant-ort01-rotated-rotated-rotated-00","expires_in":28800,"scope":"user:inference user:profile"}"#;
pub(crate) const INVALID_GRANT: &str =
    r#"{"error":"invalid_grant","error_description":"refresh token revoked"}"#;

/// A loopback token endpoint answering every POST with `status` + `body`;
/// returns its URL and the request count.
pub(crate) fn token_endpoint(status: u16, body: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback port");
    let url = format!(
        "http://{}/v1/oauth/token",
        listener.local_addr().expect("the bound address")
    );
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(read) = stream.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
            counter.fetch_add(1, Ordering::SeqCst);
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (url, hits)
}

/// One OAuth row whose access token expires `access_in` from now. Ids
/// are unique per test: the SDK remembers a refresh token Anthropic
/// rejected for the life of the process.
pub(crate) fn row(id: &str, access_in: Duration) -> Account {
    Account::new(
        id,
        Credential::Oauth(OAuthTokens {
            access: AccessToken::new(format!("sk-ant-oat01-{id}-store-access-000")),
            refresh: RefreshToken::new(format!("sk-ant-ort01-{id}-store-refresh-000")),
            expires_at: Utc::now() + access_in,
            refresh_expires_at: Some(Utc::now() + Duration::days(20)),
            scopes: vec!["user:inference".into()],
            account: None,
            organization: None,
        }),
    )
}

/// A temporary `~/.anthropic-accounts/accounts.json` holding `accounts`
/// (none: no file), and a source over it that reaches only `token_url`
/// and never Claude Code's credentials.
pub(crate) fn source_over(
    accounts: Vec<Account>,
    token_url: &str,
) -> (tempfile::TempDir, Arc<SharedStoreSource>) {
    source_configured(accounts, |config| {
        config.endpoints.token_url = token_url.to_string();
    })
}

/// [`source_over`] with the isolated configuration adjusted by `configure`
/// (a loopback usage endpoint, a sidecar in the temporary home, ...).
pub(crate) fn source_configured(
    accounts: Vec<Account>,
    configure: impl FnOnce(&mut SharedStoreConfig),
) -> (tempfile::TempDir, Arc<SharedStoreSource>) {
    let home = tempfile::tempdir().expect("a temporary home");
    let store_path = home
        .path()
        .join(".anthropic-accounts")
        .join("accounts.json");
    if !accounts.is_empty() {
        std::fs::create_dir_all(store_path.parent().expect("the store dir"))
            .expect("create the store dir");
        AccountStore {
            accounts,
            ..AccountStore::default()
        }
        .save(&store_path)
        .expect("seed the store");
    }
    let mut config = SharedStoreConfig::isolated(
        store_path,
        "http://127.0.0.1:9/v1/oauth/token",
        "http://127.0.0.1:9/api/oauth/profile",
    );
    configure(&mut config);
    (home, Arc::new(SharedStoreSource::new(config)))
}

/// A sidecar `anthropic-auth.json` holding `document` in `home`, for
/// [`SharedStoreConfig::config_path`].
pub(crate) fn sidecar(home: &std::path::Path, document: &serde_json::Value) -> std::path::PathBuf {
    let path = home.join("anthropic-auth.json");
    std::fs::write(&path, document.to_string()).expect("write the sidecar");
    path
}

/// A usage poll answer: 5h 30% (resetting in 2099), 7d 60%, and a Fable
/// weekly window at 25%.
pub(crate) const USAGE: &str = r#"{"five_hour":{"utilization":30,"resets_at":"2099-01-01T00:00:00Z"},"seven_day":{"utilization":60,"resets_at":"2099-01-05T00:00:00Z"},"limits":[{"kind":"weekly_scoped","group":"weekly","percent":25,"resets_at":"2099-01-05T00:00:00Z","scope":{"model":{"id":"claude-fable-5","display_name":"Fable"}}}]}"#;

/// A loopback endpoint that accepts connections and never answers.
pub(crate) fn hanging_endpoint() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback port");
    let url = format!(
        "http://{}/api/oauth/usage",
        listener.local_addr().expect("the bound address")
    );
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming() {
            held.push(stream);
        }
    });
    url
}

/// One request a mock endpoint received: its headers (names lowercased, in
/// order) and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedRequest {
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl CapturedRequest {
    /// The bearer token the request carried.
    pub(crate) fn bearer(&self) -> String {
        self.header("authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_string()
    }

    /// The first header named `name`.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A complete Messages stream answering "hello".
pub(crate) const OK_STREAM: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);

/// A 401 body.
pub(crate) const UNAUTHORIZED: &str =
    r#"{"type":"error","error":{"type":"authentication_error","message":"invalid token"}}"#;

/// One scripted reply: status, extra headers, body.
pub(crate) type Reply = (u16, Vec<(&'static str, String)>, &'static str);

/// A mock Messages endpoint answering requests with `replies` in order;
/// returns its base URL and the requests it received.
pub(crate) fn messages_endpoint(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<CapturedRequest>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback port");
    let base = format!(
        "http://{}",
        listener.local_addr().expect("the bound address")
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    std::thread::spawn(move || {
        for (status, extra, body) in replies {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head, body_start, length) = loop {
                let Ok(read) = stream.read(&mut chunk) else {
                    return;
                };
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    break (head, end + 4, length);
                }
                if read == 0 {
                    return;
                }
            };
            while request.len() < body_start + length {
                let Ok(read) = stream.read(&mut chunk) else {
                    return;
                };
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let headers = head
                .lines()
                .skip(1)
                .filter_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
                })
                .collect();
            seen.lock_or_recover().push(CapturedRequest {
                headers,
                body: String::from_utf8_lossy(&request[body_start..]).to_string(),
            });
            let content_type = if status == 200 {
                "text/event-stream"
            } else {
                "application/json"
            };
            let extra = extra.iter().fold(String::new(), |mut out, (name, value)| {
                out.push_str(name);
                out.push_str(": ");
                out.push_str(value);
                out.push_str("\r\n");
                out
            });
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (base, requests)
}

/// An `anthropic-messages` model of `provider` at `base_url`.
pub(crate) fn messages_model(provider: &str, base_url: &str) -> pa_types::ai::Model {
    serde_json::from_value(serde_json::json!({
        "id": "claude-opus-5-5", "name": "Claude Opus 5.5", "api": "anthropic-messages",
        "provider": provider, "baseUrl": base_url, "reasoning": true,
        "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1_000_000, "maxTokens": 128_000
    }))
    .expect("a test model")
}

/// Send one user message through pa-ai with `api_key` and return the
/// final message.
pub(crate) fn complete(
    model: &pa_types::ai::Model,
    api_key: &str,
) -> pa_ai::types::AssistantMessage {
    let context = pa_ai::types::Context {
        system_prompt: Some("You help.".to_string()),
        messages: vec![serde_json::from_value(serde_json::json!({
            "role": "user", "content": "Say hello to the world, please.", "timestamp": 0
        }))
        .expect("a user message")],
        tools: None,
    };
    let options = pa_ai::types::SimpleStreamOptions {
        base: pa_ai::types::StreamOptions {
            api_key: Some(api_key.to_string()),
            session_id: Some("session-1".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(pa_ai::complete_simple(model, &context, Some(options)))
        .expect("the anthropic provider")
}

/// The text the final message carries.
pub(crate) fn text_of(message: &pa_ai::types::AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            pa_ai::types::AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}
