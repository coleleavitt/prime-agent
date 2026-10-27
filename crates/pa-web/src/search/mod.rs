//! `web.search`: one query against a pluggable backend.
//!
//! [`SearchBackend`] is the seam: [`camoufox::CamoufoxSearch`] drives a
//! headless Camoufox (Firefox) browser over `DuckDuckGo` (no API key), and
//! [`anthropic::AnthropicSearch`] runs Anthropic's server-side `web_search`
//! tool through a nested model call (Claude Code's `WebSearch`). Every backend
//! answers a [`SearchOutcome`]:
//! the searches it ran (each a query and its hits) and any commentary, in
//! order; [`SearchOutcome::to_response`] renders the kernel's reply,
//! including Claude Code's tool-result text with its Sources reminder.

pub mod anthropic;
pub mod camoufox;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Claude Code's closing line of every search result.
pub const SOURCES_REMINDER: &str = "REMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks.";

/// Results a key-based backend asks for.
pub const RESULT_COUNT: usize = 10;

/// One validated search request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub query: String,
    /// Only results from these domains (and their subdomains).
    pub allowed_domains: Vec<String>,
    /// Never results from these domains (and their subdomains).
    pub blocked_domains: Vec<String>,
}

impl SearchRequest {
    /// Validate a kernel payload (`query`, optional `allowed_domains` /
    /// `blocked_domains` lists).
    ///
    /// # Errors
    ///
    /// Claude Code's input errors: a query under two characters, a domain
    /// list that is not a list of strings, both lists at once.
    pub fn from_payload(data: &Value) -> Result<Self, String> {
        let query = data
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if query.chars().count() < 2 {
            return Err("Error: Missing query (at least 2 characters)".to_string());
        }
        let domains = |key: &str| -> Result<Vec<String>, String> {
            match data.get(key) {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(normalize_domain)
                            .filter(|domain| !domain.is_empty())
                            .ok_or_else(|| format!("Error: {key} must be a list of domain names"))
                    })
                    .collect(),
                Some(_) => Err(format!("Error: {key} must be a list of domain names")),
            }
        };
        let allowed_domains = domains("allowed_domains")?;
        let blocked_domains = domains("blocked_domains")?;
        if !allowed_domains.is_empty() && !blocked_domains.is_empty() {
            return Err(
                "Error: Cannot specify both allowed_domains and blocked_domains in the same request"
                    .to_string(),
            );
        }
        Ok(Self {
            query,
            allowed_domains,
            blocked_domains,
        })
    }

    /// Whether `url`'s host passes the request's domain filter.
    #[must_use]
    pub fn admits(&self, url: &str) -> bool {
        let Some(host) = url::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_lowercase))
        else {
            return false;
        };
        if !self.allowed_domains.is_empty() {
            return self
                .allowed_domains
                .iter()
                .any(|domain| host_matches(&host, domain));
        }
        !self
            .blocked_domains
            .iter()
            .any(|domain| host_matches(&host, domain))
    }
}

/// A domain as a filter entry: lowercased, without a scheme, path or a
/// leading `www.`/dot.
#[must_use]
pub fn normalize_domain(domain: &str) -> String {
    let domain = domain.trim().to_lowercase();
    let domain = domain
        .split_once("://")
        .map_or(domain.as_str(), |(_, rest)| rest);
    let domain = domain.split(['/', '?', '#']).next().unwrap_or_default();
    let domain = domain.trim_start_matches('.');
    domain.strip_prefix("www.").unwrap_or(domain).to_string()
}

/// `host` is `domain` or one of its subdomains.
#[must_use]
pub fn host_matches(host: &str, domain: &str) -> bool {
    let host = host.strip_prefix("www.").unwrap_or(host);
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    /// Page text the backend returned with the hit, when any.
    pub snippet: Option<String>,
}

/// What a backend produced, in the order it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchItem {
    /// One search the backend ran: its query and hits.
    Search { query: String, hits: Vec<SearchHit> },
    /// Model commentary between or after the searches.
    Commentary(String),
    /// A search the backend reported as failed.
    Error(String),
}

/// A finished search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    /// The backend name (`anthropic`, `brave`, `exa`).
    pub backend: &'static str,
    /// `provider/id` of the model that searched, for the Anthropic backend.
    pub model: Option<String>,
    pub items: Vec<SearchItem>,
}

/// A backend's failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchError(pub String);

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SearchError {}

/// A backend's boxed future.
pub type SearchFuture = Pin<Box<dyn Future<Output = Result<SearchOutcome, SearchError>> + Send>>;

/// One search backend.
pub trait SearchBackend: Send + Sync {
    /// The backend name the outcome and telemetry carry.
    fn name(&self) -> &'static str;

    /// Run `request`; stop promptly when `cancel` fires.
    fn search(&self, request: SearchRequest, cancel: CancellationToken) -> SearchFuture;
}

/// A backend that serves from `primary` and, when it errors, from `fallback`.
/// The `Auto` choice wraps the keyless Camoufox default so it degrades to
/// Anthropic's server-side search instead of failing when the browser is
/// unavailable.
pub struct FallbackSearch {
    pub primary: Arc<dyn SearchBackend>,
    pub fallback: Option<Arc<dyn SearchBackend>>,
}

impl SearchBackend for FallbackSearch {
    fn name(&self) -> &'static str {
        self.primary.name()
    }

    fn search(&self, request: SearchRequest, cancel: CancellationToken) -> SearchFuture {
        let primary = Arc::clone(&self.primary);
        let fallback = self.fallback.clone();
        Box::pin(async move {
            match primary.search(request.clone(), cancel.clone()).await {
                Ok(outcome) => Ok(outcome),
                Err(primary_error) => match fallback {
                    Some(fallback) => fallback.search(request, cancel).await,
                    None => Err(primary_error),
                },
            }
        })
    }
}

impl SearchOutcome {
    /// Every hit, de-duplicated by URL, in order.
    #[must_use]
    pub fn hits(&self) -> Vec<&SearchHit> {
        let mut seen = std::collections::HashSet::new();
        self.items
            .iter()
            .filter_map(|item| match item {
                SearchItem::Search { hits, .. } => Some(hits),
                SearchItem::Commentary(_) | SearchItem::Error(_) => None,
            })
            .flatten()
            .filter(|hit| seen.insert(hit.url.as_str()))
            .collect()
    }

    /// The number of searches the backend ran.
    #[must_use]
    pub fn search_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(item, SearchItem::Search { .. } | SearchItem::Error(_)))
            .count()
    }

    /// Claude Code's tool-result text for this outcome.
    #[must_use]
    pub fn render_text(&self, query: &str) -> String {
        let mut text = format!("Web search results for query: \"{query}\"\n\n");
        for item in &self.items {
            match item {
                SearchItem::Commentary(commentary) => {
                    text.push_str(commentary);
                    text.push_str("\n\n");
                }
                SearchItem::Error(error) => {
                    text.push_str(error);
                    text.push_str("\n\n");
                }
                SearchItem::Search { hits, .. } if hits.is_empty() => {
                    text.push_str("No links found.\n\n");
                }
                SearchItem::Search { hits, .. } => {
                    let links: Vec<Value> = hits.iter().map(hit_json).collect();
                    text.push_str("Links: ");
                    text.push_str(&Value::Array(links).to_string());
                    text.push_str("\n\n");
                }
            }
        }
        text.push('\n');
        text.push_str(SOURCES_REMINDER);
        text.trim().to_string()
    }

    /// The kernel's reply to `web.search`.
    #[must_use]
    pub fn to_response(&self, query: &str, duration: Duration) -> Value {
        let searches: Vec<Value> = self
            .items
            .iter()
            .filter_map(|item| match item {
                SearchItem::Search { query, hits } => Some(json!({
                    "query": query,
                    "results": hits.iter().map(hit_json).collect::<Vec<_>>(),
                })),
                SearchItem::Commentary(_) | SearchItem::Error(_) => None,
            })
            .collect();
        let commentary: Vec<&str> = self
            .items
            .iter()
            .filter_map(|item| match item {
                SearchItem::Commentary(text) => Some(text.as_str()),
                SearchItem::Search { .. } | SearchItem::Error(_) => None,
            })
            .collect();
        let errors: Vec<&str> = self
            .items
            .iter()
            .filter_map(|item| match item {
                SearchItem::Error(text) => Some(text.as_str()),
                SearchItem::Search { .. } | SearchItem::Commentary(_) => None,
            })
            .collect();
        json!({
            "query": query,
            "backend": self.backend,
            "model": self.model,
            "results": self.hits().into_iter().map(hit_json).collect::<Vec<_>>(),
            "searches": searches,
            "commentary": commentary,
            "errors": errors,
            "search_count": self.search_count(),
            "duration_ms": u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            "text": self.render_text(query),
        })
    }
}

fn hit_json(hit: &SearchHit) -> Value {
    let mut value = json!({ "title": hit.title, "url": hit.url });
    if let Some(snippet) = &hit.snippet {
        value["snippet"] = json!(snippet);
    }
    value
}

/// Strip HTML tags and decode the common entities in a snippet a search API
/// returned (Brave marks matches with `<strong>`).
#[must_use]
pub fn plain_snippet(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_validate_like_claude_code() {
        assert_eq!(
            SearchRequest::from_payload(&json!({ "query": "x" })),
            Err("Error: Missing query (at least 2 characters)".to_string())
        );
        assert_eq!(
            SearchRequest::from_payload(&json!({
                "query": "rust", "allowed_domains": ["a.com"], "blocked_domains": ["b.com"]
            })),
            Err(
                "Error: Cannot specify both allowed_domains and blocked_domains in the same request"
                    .to_string()
            )
        );
        assert_eq!(
            SearchRequest::from_payload(&json!({
                "query": " rust ", "allowed_domains": ["https://www.Docs.rs/x", "github.com"],
                "blocked_domains": null
            })),
            Ok(SearchRequest {
                query: "rust".into(),
                allowed_domains: vec!["docs.rs".into(), "github.com".into()],
                blocked_domains: vec![],
            })
        );
        assert!(
            SearchRequest::from_payload(&json!({ "query": "rust", "allowed_domains": "a.com" }))
                .is_err()
        );
    }

    #[test]
    fn domain_filters_cover_subdomains_only() {
        let allowed = SearchRequest {
            query: "q".into(),
            allowed_domains: vec!["docs.rs".into()],
            blocked_domains: vec![],
        };
        assert!(allowed.admits("https://docs.rs/serde"));
        assert!(allowed.admits("https://www.docs.rs/serde"));
        assert!(allowed.admits("https://api.docs.rs/x"));
        assert!(!allowed.admits("https://notdocs.rs/x"));
        assert!(!allowed.admits("not a url"));
        let blocked = SearchRequest {
            query: "q".into(),
            allowed_domains: vec![],
            blocked_domains: vec!["example.com".into()],
        };
        assert!(!blocked.admits("https://sub.example.com/"));
        assert!(blocked.admits("https://example.org/"));
    }

    #[test]
    fn the_text_matches_claude_codes_tool_result() {
        let outcome = SearchOutcome {
            backend: "anthropic",
            model: Some("anthropic/claude-haiku-4-5".into()),
            items: vec![
                SearchItem::Commentary("Searching.".into()),
                SearchItem::Search {
                    query: "rust 2026".into(),
                    hits: vec![SearchHit {
                        title: "Rust".into(),
                        url: "https://www.rust-lang.org/".into(),
                        snippet: None,
                    }],
                },
                SearchItem::Search {
                    query: "rust news".into(),
                    hits: vec![],
                },
                SearchItem::Commentary("Rust is a language.".into()),
            ],
        };
        assert_eq!(
            outcome.render_text("rust"),
            "Web search results for query: \"rust\"\n\nSearching.\n\nLinks: \
             [{\"title\":\"Rust\",\"url\":\"https://www.rust-lang.org/\"}]\n\nNo links found.\n\n\
             Rust is a language.\n\n\nREMINDER: You MUST include the sources above in your \
             response to the user using markdown hyperlinks."
        );
        let response = outcome.to_response("rust", Duration::from_millis(12));
        assert_eq!(response["search_count"], json!(2));
        assert_eq!(
            response["results"],
            json!([{ "title": "Rust", "url": "https://www.rust-lang.org/" }])
        );
        assert_eq!(
            response["commentary"],
            json!(["Searching.", "Rust is a language."])
        );
        assert_eq!(response["duration_ms"], json!(12));
    }

    #[test]
    fn snippets_lose_markup() {
        assert_eq!(
            plain_snippet("The <strong>Rust</strong> &amp; Cargo\n book"),
            "The Rust & Cargo book"
        );
    }
}
