//! The headless-Camoufox web search backend: no API key, a real browser.
//!
//! A bundled Python script ([`SEARCH_SCRIPT`]) drives a headless Camoufox
//! (Firefox) over `DuckDuckGo`'s HTML endpoint and returns clean result URLs as
//! JSON. `DuckDuckGo`, not Google: Google wraps every result link in an opaque
//! `goto?url=...` redirect token that does not decode to a real URL, so its
//! results are useless to a search tool, while `DuckDuckGo`'s HTML endpoint
//! exposes the real destination (`uddg=`), needs no JavaScript, and does not
//! gate on consent or CAPTCHA.
//!
//! The browser must be installed in the chosen interpreter
//! (`pip install camoufox[geoip] && camoufox fetch`); when it is not, the
//! search errors and the `Auto` backend falls back to Anthropic's server-side
//! search.

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    RESULT_COUNT,
    SearchBackend,
    SearchError,
    SearchFuture,
    SearchHit,
    SearchItem,
    SearchOutcome,
    SearchRequest,
    plain_snippet,
};

/// The bundled search script, written to a temp file on first use.
const SEARCH_SCRIPT: &str = include_str!("camoufox_search.py");

/// How long one search (browser launch + navigate + scrape) may take.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(45);

/// The default interpreter when none is configured.
pub const DEFAULT_PYTHON: &str = "python3";

/// The Camoufox search backend.
#[derive(Clone, Debug)]
pub struct CamoufoxSearch {
    /// The Python interpreter that has `camoufox` installed.
    pub python: String,
}

/// The `DuckDuckGo` query: the request's query plus its domain filter as `site:`
/// operators (`DuckDuckGo` honours them; the hits are filtered again by host).
#[must_use]
pub fn ddg_query(request: &SearchRequest) -> String {
    let mut parts = vec![request.query.clone()];
    if !request.allowed_domains.is_empty() {
        let sites: Vec<String> = request
            .allowed_domains
            .iter()
            .map(|domain| format!("site:{domain}"))
            .collect();
        parts.push(format!("({})", sites.join(" OR ")));
    }
    parts.extend(
        request
            .blocked_domains
            .iter()
            .map(|domain| format!("-site:{domain}")),
    );
    parts.join(" ")
}

/// The hits of the script's JSON `results` that pass `request`'s filter.
#[must_use]
pub fn parse_hits(body: &Value, request: &SearchRequest) -> Vec<SearchHit> {
    body.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|result| {
            let url = result.get("url").and_then(Value::as_str)?;
            let snippet = result
                .get("snippet")
                .and_then(Value::as_str)
                .map(plain_snippet)
                .filter(|snippet| !snippet.is_empty());
            Some(SearchHit {
                title: result
                    .get("title")
                    .and_then(Value::as_str)
                    .map(plain_snippet)
                    .unwrap_or_default(),
                url: url.to_string(),
                snippet,
            })
        })
        .filter(|hit| request.admits(&hit.url))
        .collect()
}

/// Write the bundled script to a stable per-process temp path once.
fn script_path() -> Result<&'static std::path::Path, SearchError> {
    static PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join("prime-agent-camoufox-search.py");
        std::fs::write(&path, SEARCH_SCRIPT).ok().map(|()| path)
    })
    .as_deref()
    .ok_or_else(|| SearchError("could not write the camoufox search script".into()))
}

impl SearchBackend for CamoufoxSearch {
    fn name(&self) -> &'static str {
        "camoufox"
    }

    fn search(&self, request: SearchRequest, cancel: CancellationToken) -> SearchFuture {
        let this = self.clone();
        Box::pin(async move {
            let script = script_path()?;
            let payload = serde_json::json!({
                "query": ddg_query(&request),
                "count": RESULT_COUNT,
            })
            .to_string();
            let child = tokio::process::Command::new(&this.python)
                .arg(script)
                .arg(&payload)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| {
                    SearchError(format!(
                        "camoufox search could not start `{}`: {error}",
                        this.python
                    ))
                })?;
            // On cancel or timeout the future (and the child) drops, and
            // `kill_on_drop` tears the headless browser down.
            let output = tokio::select! {
                () = cancel.cancelled() => return Err(SearchError("cancelled".into())),
                result = tokio::time::timeout(SEARCH_TIMEOUT, child.wait_with_output()) => result
                    .map_err(|_| SearchError("camoufox search timed out".into()))?
                    .map_err(|error| SearchError(format!("camoufox search failed: {error}")))?,
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            let body: Value = serde_json::from_str(stdout.trim()).map_err(|error| {
                SearchError(format!(
                    "camoufox search returned malformed output: {error}"
                ))
            })?;
            if let Some(message) = body.get("error").and_then(Value::as_str) {
                return Err(SearchError(format!("camoufox search: {message}")));
            }
            Ok(SearchOutcome {
                backend: "camoufox",
                model: None,
                items: vec![SearchItem::Search {
                    query: request.query.clone(),
                    hits: parse_hits(&body, &request),
                }],
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn domain_filters_ride_the_query() {
        let mut request = SearchRequest {
            query: "tokio select".into(),
            allowed_domains: vec!["docs.rs".into(), "tokio.rs".into()],
            blocked_domains: vec![],
        };
        assert_eq!(
            ddg_query(&request),
            "tokio select (site:docs.rs OR site:tokio.rs)"
        );
        request.allowed_domains.clear();
        request.blocked_domains = vec!["reddit.com".into()];
        assert_eq!(ddg_query(&request), "tokio select -site:reddit.com");
    }

    #[test]
    fn hits_are_parsed_cleaned_and_filtered() {
        let request = SearchRequest {
            query: "q".into(),
            allowed_domains: vec![],
            blocked_domains: vec!["reddit.com".into()],
        };
        let body = json!({ "results": [
            { "title": "<strong>Tokio</strong>", "url": "https://tokio.rs/", "snippet": "An <strong>async</strong> runtime" },
            { "title": "Thread", "url": "https://www.reddit.com/r/rust" },
            { "title": "No url" },
        ]});
        assert_eq!(
            parse_hits(&body, &request),
            vec![SearchHit {
                title: "Tokio".into(),
                url: "https://tokio.rs/".into(),
                snippet: Some("An async runtime".into()),
            }]
        );
    }
}
