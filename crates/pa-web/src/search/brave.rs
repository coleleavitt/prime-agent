//! The Brave Search API backend (`GET /res/v1/web/search`, the key in
//! `X-Subscription-Token`). Brave has no domain-filter parameter: the
//! filter rides the query as `site:` operators and the hits are filtered
//! again by host.

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

/// Brave's web search endpoint.
pub const BRAVE_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

/// The Brave backend.
#[derive(Clone)]
pub struct BraveSearch {
    pub api_key: String,
    pub endpoint: String,
    pub client: reqwest::Client,
}

impl std::fmt::Debug for BraveSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BraveSearch")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// The query Brave receives: the request's query plus its domain filter as
/// `site:` operators.
#[must_use]
pub fn brave_query(request: &SearchRequest) -> String {
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

/// The hits of a Brave response body that pass `request`'s filter.
#[must_use]
pub fn parse_hits(body: &Value, request: &SearchRequest) -> Vec<SearchHit> {
    body.get("web")
        .and_then(|web| web.get("results"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|result| {
            let url = result.get("url").and_then(Value::as_str)?;
            let snippet = result
                .get("description")
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

/// A failed key-based API call as the error the model reads (the body's
/// first 300 characters, never the key).
pub(super) fn http_failure(service: &str, status: reqwest::StatusCode, body: &str) -> SearchError {
    let body: String = body.chars().take(300).collect();
    SearchError(format!(
        "{service} returned HTTP {}{}",
        status.as_u16(),
        if body.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", body.trim())
        }
    ))
}

impl SearchBackend for BraveSearch {
    fn name(&self) -> &'static str {
        "brave"
    }

    fn search(&self, request: SearchRequest, cancel: CancellationToken) -> SearchFuture {
        let this = self.clone();
        Box::pin(async move {
            let count = RESULT_COUNT.to_string();
            let call = this
                .client
                .get(&this.endpoint)
                .query(&[
                    ("q", brave_query(&request).as_str()),
                    ("count", count.as_str()),
                ])
                .header("Accept", "application/json")
                .header("X-Subscription-Token", &this.api_key)
                .send();
            let response = tokio::select! {
                () = cancel.cancelled() => return Err(SearchError("cancelled".into())),
                response = call => response.map_err(|error| SearchError(format!("Brave Search request failed: {error}")))?,
            };
            let status = response.status();
            let body = tokio::select! {
                () = cancel.cancelled() => return Err(SearchError("cancelled".into())),
                body = response.text() => body.map_err(|error| SearchError(format!("Brave Search response failed: {error}")))?,
            };
            if !status.is_success() {
                return Err(http_failure("Brave Search", status, &body));
            }
            let body: Value = serde_json::from_str(&body).map_err(|error| {
                SearchError(format!("Brave Search answered malformed JSON: {error}"))
            })?;
            Ok(SearchOutcome {
                backend: "brave",
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
            brave_query(&request),
            "tokio select (site:docs.rs OR site:tokio.rs)"
        );
        request.allowed_domains.clear();
        request.blocked_domains = vec!["reddit.com".into()];
        assert_eq!(brave_query(&request), "tokio select -site:reddit.com");
    }

    #[test]
    fn hits_are_parsed_cleaned_and_filtered() {
        let request = SearchRequest {
            query: "q".into(),
            allowed_domains: vec![],
            blocked_domains: vec!["reddit.com".into()],
        };
        let body = json!({ "web": { "results": [
            { "title": "<strong>Tokio</strong>", "url": "https://tokio.rs/", "description": "An <strong>async</strong> runtime" },
            { "title": "Thread", "url": "https://www.reddit.com/r/rust" },
            { "title": "No url" },
        ]}});
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
