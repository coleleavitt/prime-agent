//! The local OAuth callback server: one fresh listener per candidate port on the registered
//! redirect ports; the login flow races the browser callback against a manual paste. The served
//! pages keep the TS product's success/error wording.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// `PI_OAUTH_CALLBACK_HOST` (default `127.0.0.1`).
fn callback_host() -> String {
    std::env::var("PI_OAUTH_CALLBACK_HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// A range (not one port) so a leaked or concurrent login cannot wedge all logins with one occupied
/// socket. Distinct from the Anthropic callback port (53692).
const CALLBACK_PORT_BASE: u16 = 53_700;
const CALLBACK_PORT_COUNT: u16 = 10;
const CALLBACK_PATH: &str = "/callback";

fn redirect_uri_for(port: u16) -> String {
    format!("http://localhost:{port}{CALLBACK_PATH}")
}

/// Every redirect URI a login registers (dynamic client registration
/// offers them all).
pub fn all_redirect_uris() -> Vec<String> {
    CallbackPorts::registered()
        .candidates
        .iter()
        .map(|port| redirect_uri_for(*port))
        .collect()
}

/// The ports a callback server tries, in order; the first free one wins. Product logins use the
/// registered range (the redirect URIs dynamic registration offers). Port 0 asks the OS for an
/// ephemeral port, which tests use so parallel runs never contend for the registered range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackPorts {
    candidates: Vec<u16>,
}

impl CallbackPorts {
    /// The registered range `53700-53709`.
    #[must_use]
    pub fn registered() -> Self {
        Self {
            candidates: (0..CALLBACK_PORT_COUNT)
                .map(|offset| CALLBACK_PORT_BASE + offset)
                .collect(),
        }
    }

    /// One OS-assigned ephemeral port.
    #[must_use]
    pub fn ephemeral() -> Self {
        Self::new(vec![0])
    }

    /// Exactly these candidates, in order.
    #[must_use]
    pub fn new(candidates: Vec<u16>) -> Self {
        Self { candidates }
    }

    /// The candidates for the all-busy error: `first-last` when contiguous (the TS wording).
    fn describe(&self) -> String {
        let contiguous = self
            .candidates
            .windows(2)
            .all(|pair| pair[0].checked_add(1) == Some(pair[1]));
        match (self.candidates.first(), self.candidates.last()) {
            (Some(first), Some(last)) if contiguous && first != last => format!("{first}-{last}"),
            _ => self
                .candidates
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

impl Default for CallbackPorts {
    fn default() -> Self {
        Self::registered()
    }
}

/// The authorization code plus the echoed `state` from the browser
/// redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackCode {
    pub code: String,
    pub state: String,
}

/// What a login waits for: [`Some`] code from the browser redirect, or `None` when the callback
/// settled without one.
pub type CallbackResult = Option<CallbackCode>;

/// Settled once per login; the first settle wins.
#[derive(Default, Debug)]
struct CallbackShared {
    result: tokio::sync::Mutex<Option<CallbackResult>>,
    notify: tokio::sync::Notify,
}

impl CallbackShared {
    async fn settle(&self, result: CallbackResult) {
        let mut slot = self.result.lock().await;
        if slot.is_none() {
            *slot = Some(result);
            drop(slot);
            self.notify.notify_waiters();
        }
    }
}

/// A running callback server: its redirect URI plus the settled result.
#[derive(Debug)]
pub struct CallbackServer {
    shared: Arc<CallbackShared>,
    port: u16,
}

impl CallbackServer {
    /// Bind the first free candidate port. All candidates busy is a hard
    /// error (the TS wording).
    pub async fn start(label: &str, ports: &CallbackPorts) -> Result<Self> {
        let host = callback_host();
        let mut last_error: Option<String> = None;
        for &candidate in &ports.candidates {
            let bound = TcpListener::bind((host.as_str(), candidate))
                .await
                .and_then(|listener| Ok((listener.local_addr()?.port(), listener)));
            let (port, listener) = match bound {
                Ok(bound) => bound,
                Err(error) => {
                    last_error = Some(format!("port {candidate}: {error}"));
                    continue;
                }
            };
            let shared = Arc::new(CallbackShared::default());
            let task_shared = Arc::clone(&shared);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let shared = Arc::clone(&task_shared);
                    tokio::spawn(async move {
                        serve_callback(stream, &shared).await;
                    });
                }
            });
            return Ok(CallbackServer { shared, port });
        }
        Err(anyhow!(
            "Could not start the OAuth callback server: ports {} are all in use. \
             Close other login attempts and retry. ({label}; {})",
            ports.describe(),
            last_error.unwrap_or_else(|| "no candidate port bound".to_string()),
        ))
    }

    /// The local URL the authorization redirect must land on.
    pub fn redirect_uri(&self) -> String {
        redirect_uri_for(self.port)
    }

    /// Wait for the browser callback to settle.
    pub async fn wait_for_code(&self) -> CallbackResult {
        loop {
            if let Some(result) = self.shared.result.lock().await.take() {
                return result;
            }
            self.shared.notify.notified().await;
        }
    }

    /// Cancel: settle the waiter with `None` so an in-flight redirect loses
    /// only because the caller chose to stop waiting.
    pub async fn cancel(&self) {
        self.shared.settle(None).await;
    }
}

/// One browser request: read it, answer it, settle the login's waiter.
async fn serve_callback(mut stream: tokio::net::TcpStream, shared: &CallbackShared) {
    let Some(request) = read_request_head(&mut stream).await else {
        let _ = write_response(
            &mut stream,
            "400 Bad Request",
            oauth_error_page("Prime Agent", "Callback route not found."),
        )
        .await;
        return;
    };
    let target = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let path = target.split('?').next().unwrap_or_default();
    if path != CALLBACK_PATH {
        let _ = write_response(
            &mut stream,
            "404 Not Found",
            oauth_error_page("Prime Agent", "Callback route not found."),
        )
        .await;
        return;
    }
    let query = target.split_once('?').map_or("", |(_, query)| query);
    let code = query_param(query, "code");
    let state = query_param(query, "state");
    let error = query_param(query, "error");
    if let Some(error) = error {
        shared.settle(None).await;
        let _ = write_response(
            &mut stream,
            "400 Bad Request",
            oauth_error_page(
                "Prime Agent",
                &format!("Prime Agent authentication failed. Error: {error}"),
            ),
        )
        .await;
        return;
    }
    match (code, state) {
        (Some(code), Some(state)) if !code.is_empty() && !state.is_empty() => {
            shared.settle(Some(CallbackCode { code, state })).await;
            let _ = write_response(
                &mut stream,
                "200 OK",
                oauth_success_page(
                    "Prime Agent authentication completed. You can close this window.",
                ),
            )
            .await;
        }
        _ => {
            shared.settle(None).await;
            let _ = write_response(
                &mut stream,
                "400 Bad Request",
                oauth_error_page("Prime Agent", "Missing code or state parameter."),
            )
            .await;
        }
    }
}

/// Read one request head (until the connection goes quiet); the callback
/// browser request carries no body worth parsing.
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = [0u8; 8192];
    let read = stream.read(&mut buffer).await.ok()?;
    if read == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buffer[..read]).to_string())
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: String,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

/// One decoded query parameter (`+` as space, percent-decoded).
fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(&key.replace('+', " ")) == name {
            return Some(percent_decode(&value.replace('+', " ")));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The callback pages: a dark minimal page carrying the login's outcome.
fn render_page(title: &str, message: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{}</title>
  <style>
    body {{ margin: 0; min-height: 100vh; display: flex; align-items: center;
           justify-content: center; padding: 24px; background: #000; color: #fff;
           font-family: ui-sans-serif, system-ui, sans-serif; text-align: center; }}
    main {{ width: 100%; max-width: 560px; }}
    h1 {{ margin: 0 0 10px; font-size: 28px; line-height: 1.15; font-weight: 650; }}
    p {{ margin: 0; line-height: 1.7; color: #a1a1aa; font-size: 15px; }}
  </style>
</head>
<body>
  <main>
    <h1>Authentication</h1>
    <p>{}</p>
  </main>
</body>
</html>"#,
        escape_html(title),
        escape_html(message),
    )
}

fn oauth_success_page(message: &str) -> String {
    render_page("Prime Agent authentication completed", message)
}

fn oauth_error_page(label: &str, message: &str) -> String {
    render_page(&format!("{label} authentication failed"), message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A candidate the test holds bound (never closed while the test runs, so no parallel test can
    /// take it): an OS-assigned port nothing else contends for.
    fn held_port() -> (std::net::TcpListener, u16) {
        let blocker = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = blocker.local_addr().unwrap().port();
        (blocker, port)
    }

    #[test]
    fn the_registered_range_is_the_registration_redirect_set() {
        assert_eq!(
            all_redirect_uris(),
            (53_700..53_710)
                .map(|port| format!("http://localhost:{port}/callback"))
                .collect::<Vec<_>>()
        );
        assert_eq!(CallbackPorts::default(), CallbackPorts::registered());
        assert_eq!(CallbackPorts::registered().describe(), "53700-53709");
    }

    #[tokio::test]
    async fn falls_back_to_the_next_free_port() {
        // The first candidate is occupied, so the login lands on a later one (a leaked login
        // cannot wedge all of them).
        let (_blocker, busy) = held_port();
        let server = CallbackServer::start("Linear", &CallbackPorts::new(vec![busy, 0]))
            .await
            .unwrap();
        assert_ne!(server.port, busy);
        assert_eq!(server.redirect_uri(), redirect_uri_for(server.port));
    }

    #[tokio::test]
    async fn all_candidates_bound_fails_clearly() {
        let (_first, first) = held_port();
        let (_second, second) = held_port();
        let error = CallbackServer::start("Linear", &CallbackPorts::new(vec![first, second]))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("Could not start the OAuth callback server: ports "),
            "{error}"
        );
        assert!(error.contains(&format!("port {second}:")), "{error}");
    }

    #[tokio::test]
    async fn callback_flow_settles_code_and_state() {
        let server = CallbackServer::start("Linear", &CallbackPorts::ephemeral())
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{}/callback?code=the-code&state=the-state",
                server.port
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            server.wait_for_code().await,
            Some(CallbackCode {
                code: "the-code".to_string(),
                state: "the-state".to_string()
            })
        );
    }

    #[tokio::test]
    async fn error_callback_settles_none() {
        let server = CallbackServer::start("Linear", &CallbackPorts::ephemeral())
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{}/callback?error=access_denied",
                server.port
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 400);
        assert!(server.wait_for_code().await.is_none());
    }

    #[tokio::test]
    async fn unknown_route_is_404_without_settling() {
        let server = CallbackServer::start("Linear", &CallbackPorts::ephemeral())
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/other", server.port))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 404);
        // Unknown routes never settle the login; cancellation does.
        server.cancel().await;
        assert!(server.wait_for_code().await.is_none());
    }

    #[test]
    fn query_and_percent_decoding() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("bad%2"), "bad%2");
        assert_eq!(
            query_param("code=1&state=2", "state"),
            Some("2".to_string())
        );
        assert_eq!(query_param("code=a+b", "code"), Some("a b".to_string()));
        assert_eq!(query_param("code", "state"), None);
    }
}
