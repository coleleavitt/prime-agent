//! The streamable-HTTP leg of an MCP connection: `rmcp`'s transport driven
//! by this crate's reqwest 0.12 + rustls client (`rmcp` 3 pins reqwest 0.13,
//! whose client feature stays off). Configured headers (the Authorization
//! header included) ride the client itself; redirects are never followed, so
//! a redirecting endpoint never receives them.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::stream::BoxStream;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, WWW_AUTHENTICATE};
use rmcp::model::{ClientJsonRpcMessage, JsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_client::{
    AuthRequiredError,
    SseError,
    StreamableHttpClient,
    StreamableHttpError,
    StreamableHttpPostResponse,
};
use sse_stream::{Sse, SseStream};

use super::error::McpSessionError;

const HEADER_SESSION_ID: &str = "Mcp-Session-Id";
const HEADER_LAST_EVENT_ID: &str = "Last-Event-Id";
const EVENT_STREAM_MIME_TYPE: &str = "text/event-stream";
const JSON_MIME_TYPE: &str = "application/json";
/// The largest single SSE event a server may send (`rmcp`'s own default).
const MAX_SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;
/// Connect/write bound of every request (the in-kernel client's httpx default).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// The minimum idle read bound: an SSE stream may sit quiet between events.
const MIN_READ_TIMEOUT: Duration = Duration::from_secs(300);

/// A reqwest client carrying one connection's headers.
#[derive(Clone)]
pub(crate) struct McpHttpClient {
    client: reqwest::Client,
}

impl McpHttpClient {
    /// A client sending `headers` on every request, whose reads outlast
    /// `call_timeout` (the session's own per-call deadline settles a call).
    pub(crate) fn new(
        headers: &[(String, String)],
        call_timeout: Option<Duration>,
    ) -> Result<Self, McpSessionError> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| McpSessionError::value("MCP HTTP headers must contain strings"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| McpSessionError::value("MCP HTTP headers must contain strings"))?;
            map.insert(name, value);
        }
        let read_timeout = call_timeout.map_or(MIN_READ_TIMEOUT, |timeout| {
            (timeout + Duration::from_secs(30)).max(MIN_READ_TIMEOUT)
        });
        let client = reqwest::Client::builder()
            .default_headers(map)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(read_timeout)
            // No idle pooling: a reused connection whose last body was not
            // drained stalls on delayed ACKs (`rmcp`'s own client default).
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|error| McpSessionError::runtime(format!("MCP HTTP client: {error}")))?;
        Ok(Self { client })
    }
}

fn apply_headers(
    mut builder: reqwest::RequestBuilder,
    auth_token: Option<String>,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> reqwest::RequestBuilder {
    if let Some(token) = auth_token {
        builder = builder.bearer_auth(token);
    }
    for (name, value) in custom_headers {
        builder = builder.header(name, value);
    }
    builder
}

fn auth_required(
    response: &reqwest::Response,
) -> Option<Result<(), StreamableHttpError<reqwest::Error>>> {
    if response.status() != StatusCode::UNAUTHORIZED {
        return None;
    }
    let header = response.headers().get(WWW_AUTHENTICATE)?;
    Some(match header.to_str() {
        Ok(header) => Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
            header.to_string(),
        ))),
        Err(_) => Err(StreamableHttpError::UnexpectedServerResponse(Cow::from(
            "invalid www-authenticate header value",
        ))),
    })
}

fn is_reply_free(message: &ClientJsonRpcMessage) -> bool {
    matches!(
        message,
        JsonRpcMessage::Notification(_) | JsonRpcMessage::Response(_) | JsonRpcMessage::Error(_)
    )
}

/// The response body as an SSE stream whose events are bounded by
/// [`MAX_SSE_EVENT_BYTES`] (counted before parsing, so an endless event
/// cannot exhaust memory).
fn bounded_sse(response: reqwest::Response) -> BoxStream<'static, Result<Sse, SseError>> {
    let mut limiter = EventSizeLimiter::default();
    let bytes = response.bytes_stream().map(move |chunk| match chunk {
        Ok(chunk) => limiter
            .admit(&chunk)
            .then_some(chunk)
            .ok_or_else(|| std::io::Error::other("SSE event exceeds the size limit")),
        Err(error) => Err(std::io::Error::other(error)),
    });
    SseStream::from_bytes_stream(bytes).boxed()
}

/// Counts the bytes of the SSE event in progress; a blank line ends it.
#[derive(Default)]
struct EventSizeLimiter {
    event_bytes: usize,
    line_bytes: usize,
    after_cr: bool,
    failed: bool,
}

impl EventSizeLimiter {
    fn admit(&mut self, chunk: &[u8]) -> bool {
        if self.failed {
            return false;
        }
        for &byte in chunk {
            let after_cr = std::mem::replace(&mut self.after_cr, byte == b'\r');
            match byte {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => {
                    if self.line_bytes == 0 {
                        self.event_bytes = 0;
                    }
                    self.line_bytes = 0;
                }
                _ => {
                    self.line_bytes += 1;
                    self.event_bytes += 1;
                    if self.event_bytes > MAX_SSE_EVENT_BYTES {
                        self.failed = true;
                        return false;
                    }
                }
            }
        }
        true
    }
}

impl StreamableHttpClient for McpHttpClient {
    type Error = reqwest::Error;

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let mut builder = self.client.get(uri.as_ref()).header(
            ACCEPT,
            format!("{EVENT_STREAM_MIME_TYPE}, {JSON_MIME_TYPE}"),
        );
        if let Some(session_id) = session_id {
            builder = builder.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        if let Some(last_event_id) = last_event_id {
            builder = builder.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        let response = apply_headers(builder, auth_token, custom_headers)
            .send()
            .await
            .map_err(StreamableHttpError::Client)?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if let Some(Err(error)) = auth_required(&response) {
            return Err(error);
        }
        let response = response
            .error_for_status()
            .map_err(StreamableHttpError::Client)?;
        match response.headers().get(CONTENT_TYPE) {
            Some(content_type)
                if content_type
                    .as_bytes()
                    .starts_with(EVENT_STREAM_MIME_TYPE.as_bytes())
                    || content_type
                        .as_bytes()
                        .starts_with(JSON_MIME_TYPE.as_bytes()) => {}
            other => {
                return Err(StreamableHttpError::UnexpectedContentType(other.map(
                    |value| String::from_utf8_lossy(value.as_bytes()).to_string(),
                )));
            }
        }
        Ok(bounded_sse(response))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let builder = self
            .client
            .delete(uri.as_ref())
            .header(HEADER_SESSION_ID, session_id.as_ref());
        let response = apply_headers(builder, auth_token, custom_headers)
            .send()
            .await
            .map_err(StreamableHttpError::Client)?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Ok(());
        }
        response
            .error_for_status()
            .map_err(StreamableHttpError::Client)?;
        Ok(())
    }

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let mut builder = self.client.post(uri.as_ref()).header(
            ACCEPT,
            format!("{EVENT_STREAM_MIME_TYPE}, {JSON_MIME_TYPE}"),
        );
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            builder = builder.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let response = apply_headers(builder, auth_token, custom_headers)
            .json(&message)
            .send()
            .await
            .map_err(StreamableHttpError::Client)?;
        if let Some(Err(error)) = auth_required(&response) {
            return Err(error);
        }
        let status = response.status();
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).to_string());
        let session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string);
        // Spec: 202 for these; some servers answer an empty 200.
        if status.is_success() && response.content_length() == Some(0) && is_reply_free(&message) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let is_json = content_type
            .as_deref()
            .is_some_and(|value| value.starts_with(JSON_MIME_TYPE));
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<failed to read response body>".to_string());
            // A JSON-RPC error body is the server's real answer.
            if is_json {
                if let Ok(message @ JsonRpcMessage::Error(_)) =
                    serde_json::from_str::<ServerJsonRpcMessage>(&body)
                {
                    return Ok(StreamableHttpPostResponse::Json(message, session_id));
                }
            }
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {body}"),
            )));
        }
        if content_type
            .as_deref()
            .is_some_and(|value| value.starts_with(EVENT_STREAM_MIME_TYPE))
        {
            return Ok(StreamableHttpPostResponse::Sse(
                bounded_sse(response),
                session_id,
            ));
        }
        if !is_json {
            return Err(StreamableHttpError::UnexpectedContentType(content_type));
        }
        let body = response
            .bytes()
            .await
            .map_err(StreamableHttpError::Client)?;
        match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
            Ok(parsed) => Ok(StreamableHttpPostResponse::Json(parsed, session_id)),
            // A reply-free POST does not await an answer: an unusable body is
            // still accepted. A request needs its reply, so it fails.
            Err(_) if is_reply_free(&message) => Ok(StreamableHttpPostResponse::Accepted),
            Err(error) => Err(StreamableHttpError::Deserialize(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_limit_counts_bytes_since_the_last_blank_line() {
        let mut limiter = EventSizeLimiter::default();
        let event = vec![b'x'; MAX_SSE_EVENT_BYTES];
        assert!(limiter.admit(&event));
        assert!(limiter.admit(b"\r\n\r\n"));
        // A fresh event starts the count again.
        assert!(limiter.admit(&event));
        assert!(!limiter.admit(b"y"));
        // Once refused, the stream stays refused.
        assert!(!limiter.admit(b"\n\n"));
    }

    #[test]
    fn a_single_line_break_does_not_end_an_event() {
        let mut limiter = EventSizeLimiter::default();
        let half = vec![b'x'; MAX_SSE_EVENT_BYTES / 2 + 1];
        assert!(limiter.admit(&half));
        assert!(limiter.admit(b"\ndata: "));
        assert!(!limiter.admit(&half));
    }
}
