//! Minimal async HTTP client plumbing for providers: one streaming HTTP request through a shared
//! `reqwest` client (TS SDKs run `maxRetries: 0`). Aborts surface as [`ProviderError::Aborted`];
//! HTTP failures as [`ProviderError::Http`]; request-send failures as
//! [`ProviderError::Connection`].

use std::sync::OnceLock;

use tokio_util::sync::CancellationToken;

use crate::utils::stream_failure::{
    stream_failure_message, ConnectionErrorKind, ConnectionErrorProfile, ProviderConnectionError,
    ProviderError, ProviderHttpError, StreamFailureError, StreamFailureInfo, StreamFailureKind,
};

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static H2_ALPN_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// The HTTP/1.1 client every provider shares, pinned with `http1_only()` so that enabling the
/// reqwest `http2` feature (bedrock) cannot change the transport of any other provider.
fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .http1_only()
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .expect("reqwest client")
    })
}

/// The TLS-ALPN client for bedrock https endpoints: HTTP/2 preferred (ALPN-negotiated), like the TS
/// default transport; cleartext bedrock endpoints go through `providers/bedrock/h2.rs` instead.
fn h2_alpn_client() -> &'static reqwest::Client {
    H2_ALPN_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .expect("reqwest h2 client")
    })
}

/// The wire transport a request is issued with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// HTTP/1.1 (the default; all providers but bedrock https).
    Http1,
    /// HTTP/2 preferred over TLS ALPN (bedrock https endpoints).
    H2Alpn,
}

/// Default silence budget between two body chunks (upstream #1362): a stream that has started and
/// then sends nothing for five minutes is broken, not slow. Armed per read, so a long response
/// that keeps arriving is never cut off.
pub const DEFAULT_STREAM_STALL_TIMEOUT_MS: u64 = 300_000;

/// Environment override for the stall budget in milliseconds (`0` disables), used when the caller
/// sets no [`crate::types::StreamOptions::stream_stall_timeout_ms`].
pub const STREAM_STALL_TIMEOUT_ENV: &str = "PRIME_AGENT_STREAM_STALL_TIMEOUT_MS";

/// Resolve the per-read stall budget: the explicit option, else the environment value, else the
/// default; `0` disables the guard, and an unparsable environment value falls back to the default.
pub(crate) fn resolve_stall_timeout(
    option_ms: Option<u64>,
    env_value: Option<&str>,
) -> Option<std::time::Duration> {
    let ms = option_ms
        .or_else(|| env_value.and_then(|value| value.trim().parse::<u64>().ok()))
        .unwrap_or(DEFAULT_STREAM_STALL_TIMEOUT_MS);
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// [`resolve_stall_timeout`] against the process environment.
pub(crate) fn stall_timeout_from_env(option_ms: Option<u64>) -> Option<std::time::Duration> {
    let env_value = std::env::var(STREAM_STALL_TIMEOUT_ENV).ok();
    resolve_stall_timeout(option_ms, env_value.as_deref())
}

/// The failure a silent stream settles with: a transient server-side failure (the retry ladder
/// re-issues the request) that names the budget, so a stall is legible in the transcript.
pub(crate) fn stream_stall_failure(budget: std::time::Duration) -> ProviderError {
    let info = StreamFailureInfo {
        kind: StreamFailureKind::ServerError,
        provider_error_type: Some("stream_stall".to_string()),
        ..StreamFailureInfo::unknown()
    };
    let detail = format!(
        "the stream sent no data for {}ms and was abandoned",
        budget.as_millis()
    );
    ProviderError::StreamFailure(StreamFailureError {
        message: stream_failure_message(&info, Some(&detail)),
        info,
    })
}

/// Await one body read, racing the cancel signal and the per-read stall budget.
pub(crate) async fn read_within_stall_budget<T>(
    read: impl std::future::Future<Output = T>,
    signal: Option<&CancellationToken>,
    stall: Option<std::time::Duration>,
) -> Result<T, ProviderError> {
    let bounded = async {
        match stall {
            Some(budget) => tokio::time::timeout(budget, read)
                .await
                .map_err(|_| stream_stall_failure(budget)),
            None => Ok(read.await),
        }
    };
    match signal {
        Some(signal) => tokio::select! {
            () = signal.cancelled() => Err(ProviderError::Aborted),
            result = bounded => result,
        },
        None => bounded.await,
    }
}

/// An opened HTTP response: status, headers, and the byte stream.
pub struct HttpResponse {
    pub status: u16,
    pub headers: std::collections::HashMap<String, String>,
    body: reqwest::Response,
    signal: Option<CancellationToken>,
    /// The request's connection-error profile: body-read failures on the AWS http2 profile surface
    /// the TS bedrock transport's mid-stream texts.
    pub(crate) connection: ConnectionErrorProfile,
    /// The incomplete UTF-8 sequence that ended the previous chunk, carried into the next one by
    /// [`HttpResponse::next_text`].
    utf8_carry: Vec<u8>,
    /// The per-read silence budget (`None` disables it).
    stall: Option<std::time::Duration>,
}

/// Decode `bytes` after the carried partial sequence: complete characters are returned, an
/// incomplete trailing sequence stays in `carry` for the next chunk, and invalid bytes decode to
/// U+FFFD as `String::from_utf8_lossy` would.
fn decode_utf8_carrying(carry: &mut Vec<u8>, bytes: &[u8]) -> String {
    carry.extend_from_slice(bytes);
    let mut out = String::with_capacity(carry.len());
    let mut start = 0;
    loop {
        match std::str::from_utf8(&carry[start..]) {
            Ok(text) => {
                out.push_str(text);
                start = carry.len();
                break;
            }
            Err(error) => {
                let valid_end = start + error.valid_up_to();
                out.push_str(&String::from_utf8_lossy(&carry[start..valid_end]));
                let Some(invalid_len) = error.error_len() else {
                    start = valid_end;
                    break;
                };
                out.push(char::REPLACEMENT_CHARACTER);
                start = valid_end + invalid_len;
            }
        }
    }
    carry.drain(..start);
    out
}

impl HttpResponse {
    /// Read the next text chunk from the body (None at end of stream).
    pub async fn next_text(&mut self) -> Result<Option<String>, ProviderError> {
        if self
            .signal
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(ProviderError::Aborted);
        }
        let chunk =
            read_within_stall_budget(self.body.chunk(), self.signal.as_ref(), self.stall).await?;
        match chunk {
            Ok(Some(bytes)) => Ok(Some(decode_utf8_carrying(&mut self.utf8_carry, &bytes))),
            // A sequence still incomplete at end of stream is flushed lossily, not dropped.
            Ok(None) if !self.utf8_carry.is_empty() => {
                let rest = std::mem::take(&mut self.utf8_carry);
                Ok(Some(String::from_utf8_lossy(&rest).into_owned()))
            }
            Ok(None) => Ok(None),
            Err(error) => Err(self.body_error(&error)),
        }
    }

    /// Read the next raw byte chunk from the body (None at end of stream).
    pub async fn next_bytes(&mut self) -> Result<Option<Vec<u8>>, ProviderError> {
        if self
            .signal
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
        {
            return Err(ProviderError::Aborted);
        }
        let chunk =
            read_within_stall_budget(self.body.chunk(), self.signal.as_ref(), self.stall).await?;
        match chunk {
            Ok(Some(bytes)) => Ok(Some(bytes.to_vec())),
            Ok(None) => Ok(None),
            Err(error) => Err(self.body_error(&error)),
        }
    }

    /// Classify a body-read failure: the AWS bedrock http2 profile surfaces the TS transport's
    /// mid-stream texts (the AWS SDK appends its deserialization hint); every other provider keeps
    /// the generic body-read error.
    fn body_error(&self, error: &reqwest::Error) -> ProviderError {
        if let ConnectionErrorProfile::AwsHttp2 { .. } = self.connection {
            let failure = crate::utils_inner::h2_classify::classify_reqwest_error(error);
            return ProviderError::Connection(ProviderConnectionError {
                kind: ConnectionErrorKind::H2MidStream(failure),
                profile: self.connection.clone(),
                cause: error.to_string(),
            });
        }
        ProviderError::Http(ProviderHttpError {
            message: format!("Failed to read provider response body: {error}"),
            status: Some(self.status),
            body: None,
            headers: self.headers.clone(),
            request_id: None,
            sdk_name: None,
            retry_after_ms: None,
            provider_error_type: None,
        })
    }

    /// Read the entire body as text (for error responses and small payloads).
    pub async fn read_all_text(&mut self) -> Result<String, ProviderError> {
        let mut out = String::new();
        while let Some(chunk) = self.next_text().await? {
            out.push_str(&chunk);
        }
        Ok(out)
    }
}

pub struct RequestOptions {
    pub method: reqwest::Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub signal: Option<CancellationToken>,
    pub timeout_ms: Option<u64>,
    /// Per-read body stall budget in ms (`Some(0)` disables); `None` resolves through
    /// [`STREAM_STALL_TIMEOUT_ENV`] and then [`DEFAULT_STREAM_STALL_TIMEOUT_MS`].
    pub stall_timeout_ms: Option<u64>,
    /// The provider family's connection-error shape (fixed texts, names, and error codes the TS
    /// binary surfaces per SDK); the openai/anthropic `Sdk` default covers the Stainless-generated
    /// SDK family.
    pub connection: ConnectionErrorProfile,
    /// The wire transport (HTTP/1.1 by default; bedrock https requests use ALPN-negotiated HTTP/2).
    pub transport: Transport,
}

impl RequestOptions {
    /// Request options with the openai/anthropic `Sdk` connection profile.
    pub fn new(method: reqwest::Method, url: String) -> Self {
        Self {
            method,
            url,
            headers: Vec::new(),
            body: None,
            signal: None,
            timeout_ms: None,
            stall_timeout_ms: None,
            connection: ConnectionErrorProfile::Sdk,
            transport: Transport::Http1,
        }
    }
}

/// Issue a request and return the response with a streaming body. No retries: retry ownership lives
/// with the caller (agent layer), matching the TS `maxRetries: 0` client configuration.
pub async fn send(request: RequestOptions) -> Result<HttpResponse, ProviderError> {
    let signal = request.signal.clone();
    if signal
        .as_ref()
        .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    {
        return Err(ProviderError::Aborted);
    }

    let client = match request.transport {
        Transport::Http1 => client(),
        Transport::H2Alpn => h2_alpn_client(),
    };
    let mut builder = client.request(request.method, &request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }

    let send_future = builder.send();
    let response = match (&signal, request.timeout_ms) {
        (Some(signal), _) => {
            tokio::select! {
                () = signal.cancelled() => return Err(ProviderError::Aborted),
                result = send_future => result,
            }
        }
        (None, Some(timeout_ms)) => {
            match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), send_future)
                .await
            {
                Ok(result) => result,
                Err(_) => {
                    return Err(ProviderError::Connection(ProviderConnectionError {
                        kind: ConnectionErrorKind::Timeout,
                        profile: request.connection.clone(),
                        cause: format!("request exceeded the {timeout_ms}ms timeout"),
                    }))
                }
            }
        }
        (None, None) => send_future.await,
    };

    // The TS SDKs surface request-send failures as their fixed connection error texts: the
    // openai/anthropic SDK family throws `APIConnectionError` ("Connection error.") /
    // `APIConnectionTimeoutError` ("Request timed out.") for every fetch failure; providers whose
    // SDK appends the raw cause (mistral) or surfaces undici's raw text (codex, bedrock, google:
    // "fetch failed") rewrite it at their catch site.
    let response = response.map_err(|error| {
        let kind = if error.is_timeout() {
            ConnectionErrorKind::Timeout
        } else if error.is_connect() {
            ConnectionErrorKind::Connect
        } else if matches!(request.connection, ConnectionErrorProfile::AwsHttp2 { .. }) {
            // Pre-response http2 failure on the bedrock https transport: the h2 failure detail (no
            // deserialization hint — no response yet).
            ConnectionErrorKind::H2Request(crate::utils_inner::h2_classify::classify_reqwest_error(
                &error,
            ))
        } else {
            // The peer closed or reset after the connection was established but before the response
            // arrived (only distinguishable from refused connects on the AWS handler surfaces).
            ConnectionErrorKind::Reset
        };
        ProviderError::Connection(ProviderConnectionError {
            kind,
            profile: request.connection.clone(),
            cause: error.to_string(),
        })
    })?;

    let status = response.status().as_u16();
    let mut headers = std::collections::HashMap::new();
    for (name, value) in response.headers() {
        if let Ok(value) = value.to_str() {
            headers.insert(name.as_str().to_ascii_lowercase(), value.to_string());
        }
    }

    Ok(HttpResponse {
        status,
        headers,
        body: response,
        signal,
        connection: request.connection,
        utf8_carry: Vec::new(),
        stall: stall_timeout_from_env(request.stall_timeout_ms),
    })
}

/// JSON POST helper used by non-streaming calls (OAuth token refresh, catalogs).
#[allow(dead_code)] // token refresh/catalog fetches for upcoming providers
pub async fn post_json(
    url: &str,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
    signal: Option<CancellationToken>,
) -> Result<(u16, serde_json::Value), ProviderError> {
    let mut response = send(RequestOptions {
        headers,
        body: Some(body.to_string()),
        signal,
        timeout_ms: Some(30_000),
        ..RequestOptions::new(reqwest::Method::POST, url.to_string())
    })
    .await?;
    let status = response.status;
    let text = response.read_all_text().await?;
    let parsed = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text)
            .map_err(|error| ProviderError::Message(format!("Invalid JSON response: {error}")))?
    };
    Ok((status, parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_mock_http::{serve, MockResponse};

    /// Read the whole body chunk by chunk through `next_text`.
    async fn read_text_chunks(frames: Vec<Vec<u8>>) -> String {
        let server = serve(vec![MockResponse {
            status: 200,
            content_type: "text/event-stream",
            frames,
            hold_open: false,
        }])
        .await;
        let mut response = send(RequestOptions::new(reqwest::Method::GET, server.base_url()))
            .await
            .expect("mock response");
        response.read_all_text().await.expect("body")
    }

    /// A multi-byte character split across network chunks decodes intact instead of turning
    /// into two U+FFFD replacement characters.
    #[tokio::test]
    async fn a_character_split_across_chunks_decodes_intact() {
        let text = "data: café 日本 🦀\n\n";
        let bytes = text.as_bytes();
        let e_acute = text.find('é').unwrap();
        let kanji = text.find('日').unwrap();
        let crab = text.find('🦀').unwrap();
        let frames = vec![
            bytes[..=e_acute].to_vec(),
            bytes[e_acute + 1..kanji + 2].to_vec(),
            bytes[kanji + 2..=crab].to_vec(),
            bytes[crab + 1..crab + 2].to_vec(),
            bytes[crab + 2..].to_vec(),
        ];
        assert_eq!(read_text_chunks(frames).await, text);
    }

    /// Genuinely invalid bytes still decode lossily (one U+FFFD each), and a truncated
    /// sequence at end of stream is flushed as a replacement character rather than dropped.
    #[tokio::test]
    async fn invalid_and_truncated_bytes_still_decode_lossily() {
        let frames = vec![b"a\xffb".to_vec(), b"c\xe6\x97".to_vec()];
        assert_eq!(read_text_chunks(frames).await, "a\u{FFFD}bc\u{FFFD}");
    }
}

#[cfg(test)]
mod stall_tests {
    use std::time::Duration;

    use super::*;

    /// Upstream #1362's resolution: the explicit option wins, then the environment, then the
    /// 300 s default; `0` (or an unparsable environment value's absence of meaning) disables or
    /// falls back respectively.
    #[test]
    fn the_stall_budget_resolves_option_then_env_then_default() {
        let default = Some(Duration::from_millis(DEFAULT_STREAM_STALL_TIMEOUT_MS));
        assert_eq!(
            [
                resolve_stall_timeout(Some(1_000), Some("5")),
                resolve_stall_timeout(Some(0), Some("5")),
                resolve_stall_timeout(None, Some("2500")),
                resolve_stall_timeout(None, Some("0")),
                resolve_stall_timeout(None, Some(" nope ")),
                resolve_stall_timeout(None, None),
            ],
            [
                Some(Duration::from_millis(1_000)),
                None,
                Some(Duration::from_millis(2_500)),
                None,
                default,
                default,
            ]
        );
    }

    /// The budget is per read, not per stream: a stream that keeps arriving within the budget is
    /// never cut off however long it runs; only silence longer than the budget fails.
    #[tokio::test(start_paused = true)]
    async fn the_budget_bounds_each_read_not_the_whole_stream() {
        let budget = Some(Duration::from_millis(200));
        for _ in 0..5 {
            let read = tokio::time::sleep(Duration::from_millis(150));
            assert!(read_within_stall_budget(read, None, budget).await.is_ok());
        }
        let silent = std::future::pending::<()>();
        assert_eq!(
            read_within_stall_budget(silent, None, budget)
                .await
                .map_err(|error| error.to_string()),
            Err(stream_stall_failure(Duration::from_millis(200)).to_string())
        );
    }

    /// The cancel signal still wins over a pending read.
    #[tokio::test(start_paused = true)]
    async fn the_cancel_signal_still_aborts_a_pending_read() {
        let signal = CancellationToken::new();
        signal.cancel();
        let read = std::future::pending::<()>();
        assert!(matches!(
            read_within_stall_budget(read, Some(&signal), Some(Duration::from_secs(1))).await,
            Err(ProviderError::Aborted)
        ));
    }
}
