//! # pa-web
//!
//! The host half of `rlm.web_search` and `rlm.web_fetch`, Claude Code's
//! `WebSearch` and `WebFetch` for a kernel harness. The kernel runtime sends
//! `web.search` (`query`, `allowed_domains`, `blocked_domains`) and
//! `web.fetch` (`url`, `prompt`, `offset`) host requests; this crate serves
//! them in the session worker, never on the TUI's paint path.
//!
//! - Search runs on a pluggable [`search::SearchBackend`]: Anthropic's
//!   server-side `web_search` tool through the session's Anthropic login or
//!   key (the default when one exists), else Brave or Exa with an API key
//!   (`config`).
//! - Fetch is a plain HTTP GET (`fetch`) converted to Markdown (`html`); a
//!   `prompt` is answered over the page by the configured small model
//!   (`web.fetchModel`, else `auxiliaryModel`), and without one the page
//!   text comes back, windowed with a note on how to read on (`render`).
//!
//! The crate plugs into sessions through
//! [`pa_core::features::SessionFeature`]; see `README.md`.

pub mod config;
pub mod fetch;
pub mod html;
pub mod model;
pub mod render;
pub mod search;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::bail;
use pa_core::features::{FeatureTelemetry, SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::{
    HostRequestHandlers,
    HostRequestPayload,
    host_handler,
    host_request_cancellation,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::{BackendChoice, WebSettings};
use crate::fetch::{FetchOptions, FetchOutcome, Fetcher};
use crate::model::{CompletionHooks, ResolvedModel};
use crate::render::{Answer, PromptHandling};
use crate::search::anthropic::AnthropicSearch;
use crate::search::brave::{BRAVE_ENDPOINT, BraveSearch};
use crate::search::exa::{EXA_ENDPOINT, ExaSearch};
use crate::search::{SearchBackend, SearchRequest};

/// The kernel host request `rlm.web_search` sends.
pub const SEARCH_REQUEST: &str = "web.search";
/// The kernel host request `rlm.web_fetch` sends.
pub const FETCH_REQUEST: &str = "web.fetch";
/// The adoption event of one search.
pub const SEARCH_EVENT: &str = "web search";
/// The adoption event of one fetch.
pub const FETCH_EVENT: &str = "web fetch";
/// The most tokens a fetch answer may use.
pub const ANSWER_MAX_TOKENS: u64 = 4_096;
/// Deadline of a key-based search API call.
const SEARCH_HTTP_TIMEOUT: Duration = Duration::from_secs(45);

/// What the setup hint says when no backend can search.
pub const NO_BACKEND_MESSAGE: &str = "web_search has no backend: log in to Anthropic (/login anthropic) or configure an Anthropic API key, or set BRAVE_API_KEY or EXA_API_KEY (or the web.braveApiKey / web.exaApiKey settings).";

/// Per-build overrides (tests, embedders): fixed backends and fetch
/// options in place of the configured ones.
#[derive(Clone, Default)]
pub struct WebOverrides {
    pub fetch: Option<FetchOptions>,
    /// Serve every search with this backend.
    pub search_backend: Option<Arc<dyn SearchBackend>>,
    /// Answer every fetch prompt with this model.
    pub answer_model: Option<ResolvedModel>,
    /// The Brave and Exa endpoints (default: the public APIs).
    pub brave_endpoint: Option<String>,
    pub exa_endpoint: Option<String>,
}

/// The HTTP clients, built on the first request (loading TLS roots stays
/// off process startup).
struct Clients {
    fetcher: Arc<Fetcher>,
    /// The key-based search APIs' client.
    search: reqwest::Client,
}

type LazyClients = Arc<OnceLock<Result<Clients, String>>>;

/// The web session feature: one fetcher (and its cache) and one HTTP
/// client for every session of the process.
#[derive(Default)]
pub struct WebFeature {
    overrides: WebOverrides,
    clients: LazyClients,
}

impl std::fmt::Debug for WebFeature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebFeature").finish_non_exhaustive()
    }
}

impl WebFeature {
    /// The product's feature.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A feature with `overrides`.
    #[must_use]
    pub fn with_overrides(overrides: WebOverrides) -> Self {
        Self {
            overrides,
            clients: LazyClients::default(),
        }
    }
}

fn build_clients(overrides: &WebOverrides) -> Result<Clients, String> {
    let fetcher = Fetcher::new(overrides.fetch.clone().unwrap_or_default())?;
    let search = reqwest::Client::builder()
        .timeout(SEARCH_HTTP_TIMEOUT)
        .user_agent(fetch::user_agent())
        .build()
        .map_err(|error| format!("the web client could not start: {error}"))?;
    Ok(Clients {
        fetcher: Arc::new(fetcher),
        search,
    })
}

impl SessionFeature for WebFeature {
    fn name(&self) -> &'static str {
        "web"
    }

    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        let session = Arc::new(SessionWeb {
            agent_dir: context.agent_dir.clone(),
            cwd: context.cwd.clone(),
            session_model: (context.model.provider.clone(), context.model.id.clone()),
            telemetry: context.telemetry.clone(),
            clients: Arc::clone(&self.clients),
            overrides: self.overrides.clone(),
        });
        let searcher = Arc::clone(&session);
        handlers.register(
            SEARCH_REQUEST,
            host_handler(move |payload: HostRequestPayload| {
                let session = Arc::clone(&searcher);
                let cancel = host_request_cancellation().unwrap_or_default();
                async move { session.search(&payload.data, cancel).await }
            }),
        );
        handlers.register(
            FETCH_REQUEST,
            host_handler(move |payload: HostRequestPayload| {
                let session = Arc::clone(&session);
                let cancel = host_request_cancellation().unwrap_or_default();
                async move { session.fetch(&payload.data, cancel).await }
            }),
        );
    }
}

/// One session's view of the feature.
struct SessionWeb {
    agent_dir: PathBuf,
    cwd: PathBuf,
    session_model: (String, String),
    telemetry: Option<FeatureTelemetry>,
    clients: LazyClients,
    overrides: WebOverrides,
}

/// Pick the backend `settings` select. Reads the model registry: call it
/// off the async runtime.
///
/// # Errors
///
/// No backend can serve (the message says how to configure one), or the
/// configured one cannot (its reason).
pub fn resolve_backend(
    settings: &WebSettings,
    agent_dir: &std::path::Path,
    cwd: &std::path::Path,
    session_model: (&str, &str),
    client: &reqwest::Client,
    endpoints: (&str, &str),
) -> Result<Arc<dyn SearchBackend>, String> {
    let (brave_endpoint, exa_endpoint) = endpoints;
    let anthropic = || {
        model::resolve_search_model(
            agent_dir,
            cwd,
            settings.search_model.as_deref(),
            settings.allowed_models.as_deref(),
            session_model,
        )
        .map(|resolved| {
            resolved.map(|model| Arc::new(AnthropicSearch { model }) as Arc<dyn SearchBackend>)
        })
    };
    let brave = || {
        settings.brave_api_key.as_ref().map(|key| {
            Arc::new(BraveSearch {
                api_key: key.clone(),
                endpoint: brave_endpoint.to_string(),
                client: client.clone(),
            }) as Arc<dyn SearchBackend>
        })
    };
    let exa = || {
        settings.exa_api_key.as_ref().map(|key| {
            Arc::new(ExaSearch {
                api_key: key.clone(),
                endpoint: exa_endpoint.to_string(),
                client: client.clone(),
            }) as Arc<dyn SearchBackend>
        })
    };
    match settings.search_backend {
        BackendChoice::Anthropic => anthropic()?.ok_or_else(|| {
            "web.searchBackend is anthropic, but no Anthropic model has a login or key: run /login anthropic or set ANTHROPIC_API_KEY.".to_string()
        }),
        BackendChoice::Brave => brave().ok_or_else(|| {
            "web.searchBackend is brave, but no key is set: set BRAVE_API_KEY or the web.braveApiKey setting.".to_string()
        }),
        BackendChoice::Exa => exa().ok_or_else(|| {
            "web.searchBackend is exa, but no key is set: set EXA_API_KEY or the web.exaApiKey setting.".to_string()
        }),
        BackendChoice::Auto => {
            if let Some(backend) = anthropic()? {
                return Ok(backend);
            }
            brave()
                .or_else(exa)
                .ok_or_else(|| NO_BACKEND_MESSAGE.to_string())
        }
    }
}

fn duration_ms(duration: Duration) -> Value {
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .into()
}

impl SessionWeb {
    fn clients(&self) -> Result<&Clients, String> {
        self.clients
            .get_or_init(|| build_clients(&self.overrides))
            .as_ref()
            .map_err(Clone::clone)
    }

    fn track(&self, event: &str, pairs: &[(&str, Value)]) {
        let Some(telemetry) = &self.telemetry else {
            return;
        };
        let mut properties = pa_telemetry::Properties::new();
        for (key, value) in pairs {
            properties.set(key, value.clone());
        }
        telemetry.track(event, &properties);
    }

    async fn backend(&self) -> Result<Arc<dyn SearchBackend>, String> {
        if let Some(backend) = &self.overrides.search_backend {
            return Ok(Arc::clone(backend));
        }
        let agent_dir = self.agent_dir.clone();
        let cwd = self.cwd.clone();
        let session_model = self.session_model.clone();
        let client = self.clients()?.search.clone();
        let brave = self
            .overrides
            .brave_endpoint
            .clone()
            .unwrap_or_else(|| BRAVE_ENDPOINT.to_string());
        let exa = self
            .overrides
            .exa_endpoint
            .clone()
            .unwrap_or_else(|| EXA_ENDPOINT.to_string());
        tokio::task::spawn_blocking(move || {
            let settings = WebSettings::load(&cwd, &agent_dir);
            if let Some(unknown) = &settings.unknown_backend {
                tracing::warn!(backend = %unknown, "web.searchBackend names no backend; using auto");
            }
            resolve_backend(
                &settings,
                &agent_dir,
                &cwd,
                (&session_model.0, &session_model.1),
                &client,
                (&brave, &exa),
            )
        })
        .await
        .map_err(|error| format!("the backend lookup failed: {error}"))?
    }

    async fn search(&self, data: &Value, cancel: CancellationToken) -> anyhow::Result<Value> {
        let started = Instant::now();
        let request = match SearchRequest::from_payload(data) {
            Ok(request) => request,
            Err(error) => {
                self.track(
                    SEARCH_EVENT,
                    &[
                        ("backend", "none".into()),
                        ("outcome", "invalid".into()),
                        ("duration_ms", duration_ms(started.elapsed())),
                    ],
                );
                bail!(error);
            }
        };
        let filter = if !request.allowed_domains.is_empty() {
            "allowed"
        } else if !request.blocked_domains.is_empty() {
            "blocked"
        } else {
            "none"
        };
        let backend = match self.backend().await {
            Ok(backend) => backend,
            Err(error) => {
                self.track(
                    SEARCH_EVENT,
                    &[
                        ("backend", "none".into()),
                        ("outcome", "no_backend".into()),
                        ("domain_filter", filter.into()),
                        ("duration_ms", duration_ms(started.elapsed())),
                    ],
                );
                bail!(error);
            }
        };
        let name = backend.name();
        let query = request.query.clone();
        match backend.search(request, cancel.clone()).await {
            Ok(outcome) => {
                self.track(
                    SEARCH_EVENT,
                    &[
                        ("backend", name.into()),
                        ("outcome", "ok".into()),
                        ("domain_filter", filter.into()),
                        ("result_count", outcome.hits().len().into()),
                        ("search_count", outcome.search_count().into()),
                        ("duration_ms", duration_ms(started.elapsed())),
                    ],
                );
                Ok(outcome.to_response(&query, started.elapsed()))
            }
            Err(error) => {
                let outcome = if cancel.is_cancelled() {
                    "cancelled"
                } else {
                    "error"
                };
                self.track(
                    SEARCH_EVENT,
                    &[
                        ("backend", name.into()),
                        ("outcome", outcome.into()),
                        ("domain_filter", filter.into()),
                        ("duration_ms", duration_ms(started.elapsed())),
                    ],
                );
                bail!(error);
            }
        }
    }

    async fn answer_model(&self) -> Option<ResolvedModel> {
        if let Some(model) = &self.overrides.answer_model {
            return Some(model.clone());
        }
        let agent_dir = self.agent_dir.clone();
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || {
            let settings = WebSettings::load(&cwd, &agent_dir);
            let selector = settings.answer_model_selector()?;
            match model::resolve_selector(&agent_dir, &cwd, selector, settings.allowed_models.as_deref()) {
                Ok(model) => Some(model),
                Err(error) => {
                    tracing::warn!(%error, "web.fetch answering model unusable; returning the page text");
                    None
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    async fn fetch(&self, data: &Value, cancel: CancellationToken) -> anyhow::Result<Value> {
        let started = Instant::now();
        let url = data.get("url").and_then(Value::as_str).unwrap_or_default();
        if url.trim().is_empty() {
            bail!("Error: web_fetch needs a url");
        }
        let prompt = data
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty());
        let offset = data
            .get("offset")
            .and_then(Value::as_u64)
            .map_or(0, |offset| usize::try_from(offset).unwrap_or(usize::MAX));
        let fetcher = Arc::clone(&self.clients().map_err(anyhow::Error::msg)?.fetcher);
        let outcome = match fetcher.fetch(url, cancel.clone()).await {
            Ok(outcome) => outcome,
            Err(error) => {
                self.track(
                    FETCH_EVENT,
                    &[
                        ("outcome", error.outcome().into()),
                        ("kind", "none".into()),
                        (
                            "prompt",
                            if prompt.is_some() { "given" } else { "none" }.into(),
                        ),
                        ("duration_ms", duration_ms(started.elapsed())),
                    ],
                );
                bail!(error);
            }
        };
        let (page, cached) = match &outcome {
            FetchOutcome::Page { page, cached } => (Arc::clone(page), *cached),
            FetchOutcome::Redirect { status, .. } | FetchOutcome::HttpError { status, .. } => {
                let kind = if matches!(outcome, FetchOutcome::Redirect { .. }) {
                    "redirect"
                } else {
                    "http_error"
                };
                self.track(
                    FETCH_EVENT,
                    &[
                        ("outcome", kind.into()),
                        ("kind", "none".into()),
                        ("status_code", (*status).into()),
                        (
                            "prompt",
                            if prompt.is_some() { "given" } else { "none" }.into(),
                        ),
                        ("duration_ms", duration_ms(started.elapsed())),
                    ],
                );
                return Ok(
                    render::non_page_response(&outcome, prompt, started.elapsed())
                        .unwrap_or(Value::Null),
                );
            }
        };
        let handling = match prompt {
            None => PromptHandling::None,
            Some(_) if page.kind == fetch::ContentKind::Binary => PromptHandling::None,
            Some(prompt) => self.answer(&page, offset, prompt, cancel).await,
        };
        let response = render::page_response(&page, cached, offset, &handling, started.elapsed());
        let prompt_outcome = match &handling {
            PromptHandling::None if prompt.is_some() => "given",
            PromptHandling::None => "none",
            PromptHandling::Answered(_) => "answered",
            PromptHandling::NoModel => "no_model",
            PromptHandling::Failed(_) => "failed",
        };
        self.track(
            FETCH_EVENT,
            &[
                ("outcome", "page".into()),
                ("kind", page.kind.as_str().into()),
                ("status_code", page.status.into()),
                ("cached", cached.into()),
                ("truncated", (!response["next_offset"].is_null()).into()),
                ("prompt", prompt_outcome.into()),
                ("duration_ms", duration_ms(started.elapsed())),
            ],
        );
        Ok(response)
    }

    async fn answer(
        &self,
        page: &fetch::FetchedPage,
        offset: usize,
        prompt: &str,
        cancel: CancellationToken,
    ) -> PromptHandling {
        let (shown, start, _) = render::window(&page.content, offset);
        if shown.is_empty() {
            return PromptHandling::None;
        }
        let Some(model) = self.answer_model().await else {
            return PromptHandling::NoModel;
        };
        let total = page.content.chars().count();
        let request = render::answer_prompt(shown, start, total, prompt);
        match model::complete(
            &model,
            None,
            request,
            Some(ANSWER_MAX_TOKENS),
            CompletionHooks::default(),
            cancel,
        )
        .await
        {
            Ok(completion) if !completion.text.trim().is_empty() => {
                PromptHandling::Answered(Answer {
                    text: completion.text.trim().to_string(),
                    model: format!("{}/{}", model.model.provider, model.model.id),
                })
            }
            Ok(_) => PromptHandling::Failed("the model returned no text".to_string()),
            Err(error) => PromptHandling::Failed(error),
        }
    }
}
