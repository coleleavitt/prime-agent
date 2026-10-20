//! The Exa search API backend (`POST /search`, the key in `x-api-key`),
//! with Exa's own domain filters and a highlight per hit as its snippet.

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::brave::http_failure;
use super::{
    RESULT_COUNT,
    SearchBackend,
    SearchError,
    SearchFuture,
    SearchHit,
    SearchItem,
    SearchOutcome,
    SearchRequest,
};

/// Exa's search endpoint.
pub const EXA_ENDPOINT: &str = "https://api.exa.ai/search";
/// The longest snippet kept from Exa's highlights.
const SNIPPET_CHARS: usize = 1_000;

/// The Exa backend.
#[derive(Clone)]
pub struct ExaSearch {
    pub api_key: String,
    pub endpoint: String,
    pub client: reqwest::Client,
}

impl std::fmt::Debug for ExaSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExaSearch")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// The request body Exa receives.
#[must_use]
pub fn exa_body(request: &SearchRequest) -> Value {
    let mut body = json!({
        "query": request.query,
        "numResults": RESULT_COUNT,
        "contents": { "highlights": { "numSentences": 3, "highlightsPerUrl": 1 } },
    });
    if !request.allowed_domains.is_empty() {
        body["includeDomains"] = json!(request.allowed_domains);
    }
    if !request.blocked_domains.is_empty() {
        body["excludeDomains"] = json!(request.blocked_domains);
    }
    body
}

/// The hits of an Exa response body that pass `request`'s filter.
#[must_use]
pub fn parse_hits(body: &Value, request: &SearchRequest) -> Vec<SearchHit> {
    body.get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|result| {
            let url = result.get("url").and_then(Value::as_str)?;
            let snippet = result
                .get("highlights")
                .and_then(Value::as_array)
                .map(|highlights| {
                    highlights
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .or_else(|| {
                    result
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
                .map(|text| text.chars().take(SNIPPET_CHARS).collect::<String>())
                .filter(|text| !text.is_empty());
            Some(SearchHit {
                title: result
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                url: url.to_string(),
                snippet,
            })
        })
        .filter(|hit| request.admits(&hit.url))
        .collect()
}

impl SearchBackend for ExaSearch {
    fn name(&self) -> &'static str {
        "exa"
    }

    fn search(&self, request: SearchRequest, cancel: CancellationToken) -> SearchFuture {
        let this = self.clone();
        Box::pin(async move {
            let call = this
                .client
                .post(&this.endpoint)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .header("x-api-key", &this.api_key)
                .body(exa_body(&request).to_string())
                .send();
            let response = tokio::select! {
                () = cancel.cancelled() => return Err(SearchError("cancelled".into())),
                response = call => response.map_err(|error| SearchError(format!("Exa request failed: {error}")))?,
            };
            let status = response.status();
            let body = tokio::select! {
                () = cancel.cancelled() => return Err(SearchError("cancelled".into())),
                body = response.text() => body.map_err(|error| SearchError(format!("Exa response failed: {error}")))?,
            };
            if !status.is_success() {
                return Err(http_failure("Exa", status, &body));
            }
            let body: Value = serde_json::from_str(&body)
                .map_err(|error| SearchError(format!("Exa answered malformed JSON: {error}")))?;
            Ok(SearchOutcome {
                backend: "exa",
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
    use super::*;

    #[test]
    fn the_body_carries_exas_domain_filters() {
        let request = SearchRequest {
            query: "q".into(),
            allowed_domains: vec!["docs.rs".into()],
            blocked_domains: vec![],
        };
        assert_eq!(
            exa_body(&request),
            json!({
                "query": "q", "numResults": 10,
                "contents": { "highlights": { "numSentences": 3, "highlightsPerUrl": 1 } },
                "includeDomains": ["docs.rs"],
            })
        );
    }

    #[test]
    fn hits_take_their_highlights_as_snippets() {
        let request = SearchRequest {
            query: "q".into(),
            allowed_domains: vec!["docs.rs".into()],
            blocked_domains: vec![],
        };
        let body = json!({ "results": [
            { "title": " Serde ", "url": "https://docs.rs/serde", "highlights": ["A  framework", "for serializing."] },
            { "title": "Off", "url": "https://example.com/" },
        ]});
        assert_eq!(
            parse_hits(&body, &request),
            vec![SearchHit {
                title: "Serde".into(),
                url: "https://docs.rs/serde".into(),
                snippet: Some("A framework for serializing.".into()),
            }]
        );
    }
}
