//! `web.fetch`'s HTTP leg, after Claude Code's `WebFetch`: URL validation
//! (no credentials, no dotless or loopback/private hosts, at most 2000
//! characters), `http` upgraded to `https`, redirects followed only within
//! the same host (a `www.` prefix aside) and otherwise reported back, a
//! size cap and a deadline, HTML converted to Markdown, and a 15-minute
//! cache of successful pages.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::html;

/// Longest URL accepted (Claude Code's limit).
pub const MAX_URL_CHARS: usize = 2_000;
/// Redirect hops followed within one host.
pub const MAX_REDIRECTS: usize = 10;
/// Largest response body read.
pub const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
/// Total cached content (bytes of converted text).
pub const CACHE_MAX_BYTES: usize = 50 * 1024 * 1024;

/// How the fetcher behaves. [`FetchOptions::default`] is the product's.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Rewrite `http://` to `https://` before fetching.
    pub upgrade_http: bool,
    /// Accept loopback, private and dotless hosts (tests' local servers).
    pub allow_local: bool,
    /// The deadline for one fetch, redirects included.
    pub timeout: Duration,
    pub max_bytes: usize,
    pub cache_ttl: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            upgrade_http: true,
            allow_local: false,
            timeout: Duration::from_mins(1),
            max_bytes: MAX_RESPONSE_BYTES,
            cache_ttl: Duration::from_mins(15),
        }
    }
}

/// How `web.fetch` acquires a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FetchBackend {
    /// A plain HTTP GET, retried through the headless browser when the site
    /// bot-blocks the GET (HTTP 403/503 or a reset connection).
    #[default]
    Auto,
    /// A plain HTTP GET only; never launch the browser.
    Http,
    /// The headless browser always, falling back to a plain GET only when the
    /// browser cannot run.
    Camoufox,
}

impl FetchBackend {
    /// Parse a configured name; `None` for an unknown one.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Self::Auto),
            "http" => Some(Self::Http),
            "camoufox" | "browser" => Some(Self::Camoufox),
            _ => None,
        }
    }
}

/// The headless-browser fetch settings, resolved per request.
#[derive(Debug, Clone)]
pub struct BrowserFetch {
    pub backend: FetchBackend,
    /// The Python interpreter that has `camoufox` installed.
    pub python: String,
}

impl Default for BrowserFetch {
    fn default() -> Self {
        Self {
            backend: FetchBackend::default(),
            python: crate::search::camoufox::DEFAULT_PYTHON.to_string(),
        }
    }
}

/// A bot-block status worth retrying through the headless browser: Cloudflare
/// and friends answer a plain GET with 403, or 503 for the "under attack"
/// interstitial. A 429 (rate limit, with `Retry-After`) and origin 5xx are not
/// bot-blocks, so they are left alone.
#[must_use]
pub fn is_block_status(status: u16) -> bool {
    matches!(status, 403 | 503)
}

/// Whether an HTTP outcome should be retried through the browser (`Auto`).
#[must_use]
fn outcome_is_blocked(outcome: &FetchOutcome) -> bool {
    matches!(outcome, FetchOutcome::HttpError { status, .. } if is_block_status(*status))
}

/// The bundled headless-browser fetch script, written to a temp file on first
/// use. See [`Fetcher::fetch_via_browser`].
const FETCH_SCRIPT: &str = include_str!("camoufox_fetch.py");

/// Write the bundled browser-fetch script to a stable per-process temp path once.
fn browser_script_path() -> Result<&'static std::path::Path, String> {
    static PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join("prime-agent-camoufox-fetch.py");
        std::fs::write(&path, FETCH_SCRIPT).ok().map(|()| path)
    })
    .as_deref()
    .ok_or_else(|| "could not write the camoufox fetch script".to_string())
}

/// What a fetched body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// HTML, converted to Markdown.
    Html,
    /// Markdown served as such.
    Markdown,
    /// Other text (plain, JSON, XML, source), as served.
    Text,
    /// A binary body: not converted, only described.
    Binary,
}

impl ContentKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Markdown => "markdown",
            Self::Text => "text",
            Self::Binary => "binary",
        }
    }
}

/// One fetched page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedPage {
    /// The URL actually requested (after the `https` upgrade).
    pub url: String,
    /// The URL that answered (after same-host redirects).
    pub final_url: String,
    pub status: u16,
    pub status_text: String,
    pub content_type: String,
    /// Body size in bytes as received.
    pub bytes: usize,
    pub kind: ContentKind,
    /// The page as Markdown or text; empty for a binary body.
    pub content: String,
}

/// What a fetch produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    Page {
        page: Arc<FetchedPage>,
        cached: bool,
    },
    /// A redirect to another host, not followed.
    Redirect {
        url: String,
        target: String,
        status: u16,
        status_text: String,
    },
    /// A non-success status; the body is not read.
    HttpError {
        url: String,
        status: u16,
        status_text: String,
        retry_after: Option<String>,
    },
}

/// Why a fetch failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    InvalidUrl(String),
    /// A URL the fetcher refuses (credentials, local hosts).
    Refused(String),
    TooLarge(usize),
    TooManyRedirects,
    Timeout(Duration),
    Cancelled,
    Transport(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(detail) => write!(f, "Invalid URL: {detail}"),
            Self::Refused(detail) => f.write_str(detail),
            Self::TooLarge(max) => write!(f, "The response is larger than {max} bytes"),
            Self::TooManyRedirects => write!(f, "Too many redirects (exceeded {MAX_REDIRECTS})"),
            Self::Timeout(deadline) => write!(
                f,
                "Fetch did not complete within the {}s deadline",
                deadline.as_secs()
            ),
            Self::Cancelled => f.write_str("The fetch was cancelled"),
            Self::Transport(detail) => write!(f, "Fetch failed: {detail}"),
        }
    }
}

impl std::error::Error for FetchError {}

impl FetchError {
    /// The telemetry outcome of this failure.
    #[must_use]
    pub fn outcome(&self) -> &'static str {
        match self {
            Self::InvalidUrl(_) | Self::Refused(_) => "refused",
            Self::Cancelled => "cancelled",
            Self::TooLarge(_) | Self::TooManyRedirects | Self::Timeout(_) | Self::Transport(_) => {
                "error"
            }
        }
    }
}

fn is_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.octets()[0] == 0
                // 100.64.0.0/10, carrier-grade NAT (tailnets).
                || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || (ip.segments()[0] & 0xffc0) == 0xfe80
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_local_ip(IpAddr::V4(v4)))
        }
    }
}

/// Validate `raw` and apply the `https` upgrade.
///
/// # Errors
///
/// An unparsable or non-http(s) URL, one over [`MAX_URL_CHARS`], one with
/// credentials, or (without `allow_local`) a dotless, loopback or private
/// host. The local-host refusal says to use `bash()` with `curl` instead,
/// as Claude Code does.
pub fn validate_url(raw: &str, options: &FetchOptions) -> Result<Url, FetchError> {
    let raw = raw.trim();
    if raw.chars().count() > MAX_URL_CHARS {
        return Err(FetchError::InvalidUrl(format!(
            "the URL is longer than {MAX_URL_CHARS} characters"
        )));
    }
    let mut url = Url::parse(raw).map_err(|error| {
        FetchError::InvalidUrl(format!("{raw:?} could not be parsed ({error})"))
    })?;
    match url.scheme() {
        "https" => {}
        "http" => {
            if options.upgrade_http {
                url.set_scheme("https").map_err(|()| {
                    FetchError::InvalidUrl(format!("{raw:?} cannot be upgraded to https"))
                })?;
            }
        }
        other => {
            return Err(FetchError::InvalidUrl(format!(
                "only http and https URLs can be fetched, not {other}:"
            )));
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FetchError::Refused(
            "web_fetch does not fetch URLs that carry credentials".to_string(),
        ));
    }
    let local_refusal = || {
        FetchError::Refused(
            "web_fetch cannot fetch localhost, private addresses or hostnames without a dot. To reach a local server, use bash() with curl instead.".to_string(),
        )
    };
    match url.host() {
        None => return Err(FetchError::InvalidUrl(format!("{raw:?} has no host"))),
        Some(_) if options.allow_local => {}
        Some(url::Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_lowercase();
            let tld = domain.rsplit('.').next().unwrap_or_default();
            if !domain.contains('.') || tld == "localhost" || tld == "local" {
                return Err(local_refusal());
            }
        }
        Some(url::Host::Ipv4(ip)) => {
            if is_local_ip(IpAddr::V4(ip)) {
                return Err(local_refusal());
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            if is_local_ip(IpAddr::V6(ip)) {
                return Err(local_refusal());
            }
        }
    }
    Ok(url)
}

fn strip_www(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

/// A redirect the fetcher follows on its own: same scheme, same port, no
/// credentials, and the same host up to a leading `www.`.
#[must_use]
pub fn is_permitted_redirect(from: &Url, to: &Url) -> bool {
    from.scheme() == to.scheme()
        && from.port_or_known_default() == to.port_or_known_default()
        && to.username().is_empty()
        && to.password().is_none()
        && match (from.host_str(), to.host_str()) {
            (Some(from), Some(to)) => {
                strip_www(&from.to_lowercase()) == strip_www(&to.to_lowercase())
            }
            _ => false,
        }
}

fn classify(content_type: &str, body: &[u8]) -> ContentKind {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if mime == "text/html" || mime == "application/xhtml+xml" {
        return ContentKind::Html;
    }
    if mime == "text/markdown" || mime == "text/x-markdown" {
        return ContentKind::Markdown;
    }
    let textual = mime.starts_with("text/")
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/ecmascript"
                | "application/toml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/x-sh"
        );
    if textual {
        return ContentKind::Text;
    }
    if mime.is_empty() {
        if let Ok(text) = std::str::from_utf8(&body[..body.len().min(4096)]) {
            let head = text.trim_start().to_lowercase();
            if head.starts_with("<!doctype html") || head.starts_with("<html") {
                return ContentKind::Html;
            }
            return ContentKind::Text;
        }
    }
    ContentKind::Binary
}

struct CacheEntry {
    stored: Instant,
    page: Arc<FetchedPage>,
}

/// The fetcher: one HTTP client and the page cache, shared by every
/// session of the process.
pub struct Fetcher {
    client: reqwest::Client,
    options: FetchOptions,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl std::fmt::Debug for Fetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetcher")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

/// The `User-Agent` fetches send.
#[must_use]
pub fn user_agent() -> String {
    format!(
        "prime-agent/{} (web_fetch; +https://github.com/PrimeIntellect-ai/prime-agent)",
        env!("CARGO_PKG_VERSION")
    )
}

impl Fetcher {
    /// A fetcher with `options`.
    ///
    /// # Errors
    ///
    /// The HTTP client could not be built (TLS roots).
    pub fn new(options: FetchOptions) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(user_agent())
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            client,
            options,
            cache: Mutex::new(HashMap::new()),
        })
    }

    #[must_use]
    pub fn options(&self) -> &FetchOptions {
        &self.options
    }

    fn cached(&self, key: &str) -> Option<Arc<FetchedPage>> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ttl = self.options.cache_ttl;
        cache.retain(|_, entry| entry.stored.elapsed() < ttl);
        cache.get(key).map(|entry| Arc::clone(&entry.page))
    }

    fn store(&self, key: String, page: Arc<FetchedPage>) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(
            key,
            CacheEntry {
                stored: Instant::now(),
                page,
            },
        );
        // Evict the oldest pages past the size budget.
        loop {
            let total: usize = cache
                .values()
                .map(|entry| entry.page.content.len().max(1))
                .sum();
            if total <= CACHE_MAX_BYTES || cache.len() <= 1 {
                break;
            }
            let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.stored)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            cache.remove(&oldest);
        }
    }

    /// Fetch `raw`: from the cache when a fresh copy exists, else through the
    /// configured [`BrowserFetch`] backend within the deadline.
    ///
    /// # Errors
    ///
    /// See [`FetchError`].
    pub async fn fetch(
        &self,
        raw: &str,
        browser: &BrowserFetch,
        cancel: CancellationToken,
    ) -> Result<FetchOutcome, FetchError> {
        let url = validate_url(raw, &self.options)?;
        let key = url.to_string();
        if let Some(page) = self.cached(&key) {
            return Ok(FetchOutcome::Page { page, cached: true });
        }
        let outcome = self.fetch_with_backend(url, browser, cancel).await?;
        if let FetchOutcome::Page { page, .. } = &outcome {
            self.store(key, Arc::clone(page));
        }
        Ok(outcome)
    }

    /// Run the configured backend: the browser first for `Camoufox`, else an
    /// HTTP GET retried through the browser on a bot-block for `Auto`.
    async fn fetch_with_backend(
        &self,
        url: Url,
        browser: &BrowserFetch,
        cancel: CancellationToken,
    ) -> Result<FetchOutcome, FetchError> {
        // Browser-first: fall back to HTTP only when the browser cannot run.
        if browser.backend == FetchBackend::Camoufox {
            match self.fetch_via_browser(&url, browser, cancel.clone()).await {
                Ok(outcome) => return Ok(outcome),
                Err(error) => {
                    tracing::warn!(%error, "camoufox fetch failed; falling back to HTTP");
                }
            }
        }
        let http = self.fetch_http(url.clone(), cancel.clone()).await;
        // Auto: retry a bot-blocked GET (or a reset connection) through the browser.
        if browser.backend == FetchBackend::Auto {
            let blocked = match &http {
                Ok(outcome) => outcome_is_blocked(outcome),
                Err(FetchError::Transport(_)) => true,
                Err(_) => false,
            };
            if blocked {
                if let Ok(outcome) = self.fetch_via_browser(&url, browser, cancel).await {
                    return Ok(outcome);
                }
            }
        }
        http
    }

    /// One HTTP GET (redirects followed within the host), bounded by the
    /// deadline and the cancellation token.
    async fn fetch_http(
        &self,
        url: Url,
        cancel: CancellationToken,
    ) -> Result<FetchOutcome, FetchError> {
        let deadline = self.options.timeout;
        tokio::select! {
            () = cancel.cancelled() => Err(FetchError::Cancelled),
            () = tokio::time::sleep(deadline) => Err(FetchError::Timeout(deadline)),
            outcome = self.fetch_uncached(url) => outcome,
        }
    }

    /// Fetch `url` through a headless Camoufox: a bundled Python script
    /// navigates the page, waits out any bot-check interstitial, and returns
    /// the rendered HTML, which is converted to Markdown like an HTTP page.
    async fn fetch_via_browser(
        &self,
        url: &Url,
        browser: &BrowserFetch,
        cancel: CancellationToken,
    ) -> Result<FetchOutcome, FetchError> {
        let script = browser_script_path().map_err(FetchError::Transport)?;
        let deadline = self.options.timeout;
        let payload = serde_json::json!({
            "url": url.as_str(),
            "timeout_ms": u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
        })
        .to_string();
        let child = tokio::process::Command::new(&browser.python)
            .arg(script)
            .arg(&payload)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                FetchError::Transport(format!(
                    "camoufox fetch could not start `{}`: {error}",
                    browser.python
                ))
            })?;
        // Give the browser a little slack past its in-script navigation deadline;
        // on cancel or timeout the child drops and `kill_on_drop` tears it down.
        let output = tokio::select! {
            () = cancel.cancelled() => return Err(FetchError::Cancelled),
            result = tokio::time::timeout(deadline + Duration::from_secs(15), child.wait_with_output()) => result
                .map_err(|_| FetchError::Timeout(deadline))?
                .map_err(|error| FetchError::Transport(format!("camoufox fetch failed: {error}")))?,
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let body: Value = serde_json::from_str(stdout.trim()).map_err(|error| {
            FetchError::Transport(format!("camoufox fetch returned malformed output: {error}"))
        })?;
        if let Some(message) = body.get("error").and_then(Value::as_str) {
            return Err(FetchError::Transport(format!("camoufox fetch: {message}")));
        }
        let html_body = body.get("html").and_then(Value::as_str).unwrap_or_default();
        if html_body.trim().is_empty() {
            return Err(FetchError::Transport(
                "camoufox fetch returned no content".into(),
            ));
        }
        // The browser follows redirects itself (cross-host included), so the
        // final URL is re-validated: a redirect to a private or loopback host is
        // refused exactly as the HTTP leg would refuse it (an SSRF guard).
        let final_raw = body
            .get("final_url")
            .and_then(Value::as_str)
            .unwrap_or(url.as_str());
        let final_url = validate_url(final_raw, &self.options)?;
        let content_type = body
            .get("content_type")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("text/html")
            .to_string();
        let bytes = html_body.len();
        let kind = classify(&content_type, html_body.as_bytes());
        let content = match kind {
            ContentKind::Html => {
                let text = html_body.to_string();
                tokio::task::spawn_blocking(move || html::to_markdown(&text))
                    .await
                    .map_err(|error| {
                        FetchError::Transport(format!("the HTML conversion failed: {error}"))
                    })?
            }
            ContentKind::Markdown | ContentKind::Text => html_body.to_string(),
            ContentKind::Binary => String::new(),
        };
        // The browser got past the block, so the page is a success regardless of
        // the interstitial status the first navigation response carried.
        Ok(FetchOutcome::Page {
            page: Arc::new(FetchedPage {
                url: url.to_string(),
                final_url: final_url.to_string(),
                status: 200,
                status_text: "OK".to_string(),
                content_type,
                bytes,
                kind,
                content,
            }),
            cached: false,
        })
    }

    async fn fetch_uncached(&self, url: Url) -> Result<FetchOutcome, FetchError> {
        let original = url.clone();
        let mut current = url;
        for _hop in 0..=MAX_REDIRECTS {
            let response = self
                .client
                .get(current.clone())
                .header("Accept", "text/markdown, text/html, */*")
                .send()
                .await
                .map_err(|error| FetchError::Transport(error.to_string()))?;
            let status = response.status();
            let status_text = status.canonical_reason().unwrap_or_default().to_string();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let Some(target) = location.and_then(|location| current.join(location).ok()) else {
                    return Ok(FetchOutcome::HttpError {
                        url: original.to_string(),
                        status: status.as_u16(),
                        status_text,
                        retry_after: None,
                    });
                };
                if is_permitted_redirect(&current, &target) {
                    current = target;
                    continue;
                }
                return Ok(FetchOutcome::Redirect {
                    url: original.to_string(),
                    target: target.to_string(),
                    status: status.as_u16(),
                    status_text,
                });
            }
            if !status.is_success() {
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .filter(|value| {
                        value.len() <= 6 && value.bytes().all(|byte| byte.is_ascii_digit())
                    })
                    .map(str::to_string);
                return Ok(FetchOutcome::HttpError {
                    url: original.to_string(),
                    status: status.as_u16(),
                    status_text,
                    retry_after,
                });
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let body = self.read_body(response).await?;
            let kind = classify(&content_type, &body);
            let bytes = body.len();
            let content = match kind {
                ContentKind::Html => {
                    let text = String::from_utf8_lossy(&body).into_owned();
                    tokio::task::spawn_blocking(move || html::to_markdown(&text))
                        .await
                        .map_err(|error| {
                            FetchError::Transport(format!("the HTML conversion failed: {error}"))
                        })?
                }
                ContentKind::Markdown | ContentKind::Text => {
                    String::from_utf8_lossy(&body).into_owned()
                }
                ContentKind::Binary => String::new(),
            };
            return Ok(FetchOutcome::Page {
                page: Arc::new(FetchedPage {
                    url: original.to_string(),
                    final_url: current.to_string(),
                    status: status.as_u16(),
                    status_text,
                    content_type,
                    bytes,
                    kind,
                    content,
                }),
                cached: false,
            });
        }
        Err(FetchError::TooManyRedirects)
    }

    async fn read_body(&self, mut response: reqwest::Response) -> Result<Vec<u8>, FetchError> {
        let max = self.options.max_bytes;
        if response
            .content_length()
            .is_some_and(|length| usize::try_from(length).map_or(true, |length| length > max))
        {
            return Err(FetchError::TooLarge(max));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| FetchError::Transport(error.to_string()))?
        {
            if body.len() + chunk.len() > max {
                return Err(FetchError::TooLarge(max));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_validate_like_claude_codes_web_fetch() {
        let options = FetchOptions::default();
        assert_eq!(
            validate_url("http://example.com/a?b=1", &options)
                .unwrap()
                .as_str(),
            "https://example.com/a?b=1"
        );
        for refused in [
            "http://localhost:8080/",
            "https://intranet/",
            "http://127.0.0.1/",
            "http://192.168.1.10/",
            "http://[::1]/",
            "http://printer.local/",
            "https://user:pass@example.com/",
        ] {
            assert!(
                matches!(validate_url(refused, &options), Err(FetchError::Refused(_))),
                "{refused} must be refused"
            );
        }
        for invalid in ["ftp://example.com/", "not a url", "file:///etc/passwd"] {
            assert!(
                matches!(
                    validate_url(invalid, &options),
                    Err(FetchError::InvalidUrl(_))
                ),
                "{invalid} is invalid"
            );
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS));
        assert!(matches!(
            validate_url(&long, &options),
            Err(FetchError::InvalidUrl(_))
        ));
        let local = FetchOptions {
            allow_local: true,
            upgrade_http: false,
            ..FetchOptions::default()
        };
        assert_eq!(
            validate_url("http://127.0.0.1:9/x", &local)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:9/x"
        );
    }

    #[test]
    fn only_same_host_redirects_are_followed() {
        let from = Url::parse("https://example.com/a").unwrap();
        let ok = |to: &str| is_permitted_redirect(&from, &Url::parse(to).unwrap());
        assert!(ok("https://www.example.com/b"));
        assert!(ok("https://example.com:443/c"));
        assert!(!ok("http://example.com/b"));
        assert!(!ok("https://other.com/"));
        assert!(!ok("https://example.com:8443/"));
        assert!(!ok("https://u@example.com/"));
    }

    #[test]
    fn the_fetch_backend_parses_and_only_bot_blocks_retry_through_the_browser() {
        assert_eq!(FetchBackend::parse(""), Some(FetchBackend::Auto));
        assert_eq!(FetchBackend::parse("AUTO"), Some(FetchBackend::Auto));
        assert_eq!(FetchBackend::parse(" http "), Some(FetchBackend::Http));
        assert_eq!(
            FetchBackend::parse("camoufox"),
            Some(FetchBackend::Camoufox)
        );
        assert_eq!(FetchBackend::parse("browser"), Some(FetchBackend::Camoufox));
        assert_eq!(FetchBackend::parse("wget"), None);
        assert_eq!(FetchBackend::default(), FetchBackend::Auto);

        // Cloudflare's bot-block statuses retry; a rate limit, a 404 and origin
        // 5xx do not.
        assert!(is_block_status(403));
        assert!(is_block_status(503));
        for not_blocked in [200, 404, 429, 500, 502, 520] {
            assert!(
                !is_block_status(not_blocked),
                "{not_blocked} is not a block"
            );
        }
        let blocked = FetchOutcome::HttpError {
            url: "https://x/".into(),
            status: 403,
            status_text: "Forbidden".into(),
            retry_after: None,
        };
        assert!(outcome_is_blocked(&blocked));
        let rate_limited = FetchOutcome::HttpError {
            url: "https://x/".into(),
            status: 429,
            status_text: "Too Many Requests".into(),
            retry_after: Some("30".into()),
        };
        assert!(!outcome_is_blocked(&rate_limited));
        let redirect = FetchOutcome::Redirect {
            url: "https://x/".into(),
            target: "https://y/".into(),
            status: 302,
            status_text: "Found".into(),
        };
        assert!(!outcome_is_blocked(&redirect));
    }

    #[test]
    fn bodies_are_classified_by_type_then_sniffed() {
        assert_eq!(classify("text/html; charset=utf-8", b""), ContentKind::Html);
        assert_eq!(classify("text/markdown", b""), ContentKind::Markdown);
        assert_eq!(classify("application/vnd.api+json", b""), ContentKind::Text);
        assert_eq!(classify("image/png", b"\x89PNG"), ContentKind::Binary);
        assert_eq!(classify("", b"<!DOCTYPE html><p>x"), ContentKind::Html);
        assert_eq!(classify("", b"plain"), ContentKind::Text);
        assert_eq!(classify("", b"\xff\xfe\x00"), ContentKind::Binary);
    }
}
