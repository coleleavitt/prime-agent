//! Azure `OpenAI` Responses API streaming provider: deployment name resolution (options/env map),
//! base-URL normalization with the /openai/v1 path, the `<base>/responses?api-version=<v>` route
//! (deployment in the body), and the shared Responses stream processor.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::env_api_keys::get_env_api_key;
use crate::event_stream::{
    AssistantMessageEvent,
    AssistantMessageEventStream,
    AssistantMessageEventWriter,
    create_assistant_message_event_stream,
    initial_assistant_message,
};
use crate::models::{clamp_thinking_level, supports_thinking};
use crate::providers::openai_responses_shared::{
    AZURE_TOOL_CALL_PROVIDERS,
    ConvertResponsesMessagesOptions,
    ConvertResponsesToolsOptions,
    ResponsesStreamHooks,
    convert_responses_messages,
    convert_responses_tools,
};
use crate::providers::openai_responses_stream::process_responses_sse;
use crate::providers::simple_options::build_base_options;
use crate::providers::stream_end::end_stream;
use crate::registry::Provider;
use crate::types::{
    AssistantMessage,
    CacheRetention,
    Context,
    Model,
    ModelExt,
    ModelThinkingLevel,
    SimpleStreamOptions,
    StopReason,
    StreamOptions,
    Usage,
};
use crate::utils_inner::diagnostics::now_ms;
use crate::utils_inner::http::{HttpResponse, RequestOptions, send};
use crate::utils_inner::stream_failure::{ProviderError, format_stream_failure_message};

pub const API_AZURE_OPENAI_RESPONSES: &str = "azure-openai-responses";

const DEFAULT_AZURE_API_VERSION: &str = "v1";

fn parse_deployment_name_map(value: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(value) = value else {
        return map;
    };
    for entry in value.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (model_id, deployment_name) = trimmed.split_once('=').unwrap_or(("", ""));
        if model_id.is_empty() || deployment_name.is_empty() {
            continue;
        }
        map.insert(
            model_id.trim().to_string(),
            deployment_name.trim().to_string(),
        );
    }
    map
}

fn resolve_deployment_name(model: &Model, options: Option<&AzureOpenAIResponsesOptions>) -> String {
    if let Some(deployment) = options.and_then(|options| options.azure_deployment_name.as_ref()) {
        return deployment.clone();
    }
    let mapped = parse_deployment_name_map(
        std::env::var("AZURE_OPENAI_DEPLOYMENT_NAME_MAP")
            .ok()
            .as_deref(),
    )
    .get(&model.id)
    .cloned();
    mapped.unwrap_or_else(|| model.id.clone())
}

/// Provider-native options.
#[derive(Clone, Default)]
pub struct AzureOpenAIResponsesOptions {
    pub base: StreamOptions,
    pub reasoning_effort: Option<ModelThinkingLevel>,
    pub reasoning_summary: Option<crate::providers::openai_responses_shared::ReasoningSummary>,
    pub azure_api_version: Option<String>,
    pub azure_resource_name: Option<String>,
    pub azure_base_url: Option<String>,
    pub azure_deployment_name: Option<String>,
}

impl AzureOpenAIResponsesOptions {
    pub fn from_base(base: StreamOptions) -> Self {
        Self {
            base,
            reasoning_effort: None,
            reasoning_summary: None,
            azure_api_version: None,
            azure_resource_name: None,
            azure_base_url: None,
            azure_deployment_name: None,
        }
    }
}

fn normalize_azure_base_url(base_url: &str) -> Result<String, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let url = url::Url::parse(trimmed)
        .map_err(|_| format!("Invalid Azure OpenAI base URL: {base_url}"))?;
    let host = url.host_str().unwrap_or_default();
    let is_azure_host =
        host.ends_with(".openai.azure.com") || host.ends_with(".cognitiveservices.azure.com");
    let normalized_path = url.path().trim_end_matches('/');

    // Ensure Azure hosts have /openai/v1 as base path so the request resolves as <base>/responses.
    if is_azure_host && (normalized_path.is_empty() || normalized_path == "/openai") {
        let mut normalized = url.clone();
        normalized.set_path("/openai/v1");
        normalized.set_query(None);
        return Ok(normalized.to_string().trim_end_matches('/').to_string());
    }

    Ok(trimmed.to_string())
}

fn build_default_base_url(resource_name: &str) -> String {
    format!("https://{resource_name}.openai.azure.com/openai/v1")
}

/// Resolves the Responses request URL. Azure's Responses API has no per-deployment route: the
/// deployment travels as `model` in the body and the URL is `<base>/responses?api-version=<v>`.
/// The query is always sent (`v1` by default), as the TS `AzureOpenAI` client did. Azure hosts
/// are normalized to `/openai/v1`, where Azure documents only `v1` and `preview`; a dated version
/// only reaches a `/openai` path on a non-Azure gateway base URL.
fn resolve_responses_url(
    model: &Model,
    options: Option<&AzureOpenAIResponsesOptions>,
) -> Result<String, String> {
    let api_version = options
        .and_then(|options| options.azure_api_version.clone())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("AZURE_OPENAI_API_VERSION")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_AZURE_API_VERSION.to_string());

    let base_url = options
        .and_then(|options| options.azure_base_url.as_ref())
        .map(|url| url.trim().to_string())
        .or_else(|| {
            std::env::var("AZURE_OPENAI_BASE_URL")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });
    let resource_name = options
        .and_then(|options| options.azure_resource_name.clone())
        .or_else(|| std::env::var("AZURE_OPENAI_RESOURCE_NAME").ok())
        .filter(|value| !value.is_empty());

    let mut resolved_base_url = base_url;
    if resolved_base_url.is_none()
        && let Some(resource) = resource_name
    {
        resolved_base_url = Some(build_default_base_url(&resource));
    }
    if resolved_base_url.is_none() && !model.base_url.is_empty() {
        resolved_base_url = Some(model.base_url.clone());
    }
    let Some(resolved_base_url) = resolved_base_url else {
        return Err(
            "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME, or pass azureBaseUrl, azureResourceName, or model.baseUrl."
                .to_string(),
        );
    };

    let base_url = normalize_azure_base_url(&resolved_base_url)?;
    let base_url = base_url.trim_end_matches('/');
    Ok(format!("{base_url}/responses?api-version={api_version}"))
}

fn build_params(
    model: &Model,
    context: &Context,
    options: &AzureOpenAIResponsesOptions,
    deployment_name: &str,
) -> Value {
    let messages = convert_responses_messages(
        model,
        context,
        &AZURE_TOOL_CALL_PROVIDERS,
        ConvertResponsesMessagesOptions::include_system_prompt(),
    );
    let mut params = Map::new();
    params.insert("model".into(), json!(deployment_name));
    params.insert("input".into(), json!(messages));
    params.insert("stream".into(), json!(true));
    // TS #2948: Azure stores responses server-side by default — pin `store: false`, and drop
    // `prompt_cache_key` when the caller pinned cacheRetention to none; Azure does not send
    // `prompt_cache_retention` (support unclear) — that stays as-is.
    if options.base.cache_retention != Some(CacheRetention::None)
        && let Some(session_id) = &options.base.session_id
    {
        params.insert("prompt_cache_key".into(), json!(session_id));
    }
    params.insert("store".into(), json!(false));
    if let Some(max_tokens) = options.base.max_tokens {
        params.insert("max_output_tokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = options.base.temperature {
        params.insert("temperature".into(), json!(temperature));
    }
    if let Some(tools) = &context.tools
        && !tools.is_empty()
    {
        params.insert(
            "tools".into(),
            json!(convert_responses_tools(
                tools,
                ConvertResponsesToolsOptions { strict: None }
            )),
        );
    }
    if supports_thinking(model) {
        if options.reasoning_effort.is_some() || options.reasoning_summary.is_some() {
            let effort = match options.reasoning_effort {
                Some(effort) => model
                    .thinking_level_map_value(effort)
                    .and_then(Option::<&String>::cloned)
                    .unwrap_or_else(|| effort.wire_name().to_string()),
                None => "medium".to_string(),
            };
            let summary = options.reasoning_summary.map_or(
                "auto",
                super::openai_responses_hooks::ReasoningSummary::as_str,
            );
            params.insert(
                "reasoning".into(),
                json!({ "effort": effort, "summary": summary }),
            );
            params.insert("include".into(), json!(["reasoning.encrypted_content"]));
        } else {
            let off_null = model
                .thinking_level_map_value(ModelThinkingLevel::Off)
                .is_some_and(|value| value.is_none());
            if !off_null {
                let off_value = model
                    .thinking_level_map_value(ModelThinkingLevel::Off)
                    .and_then(Option::<&String>::cloned)
                    .unwrap_or_else(|| "none".to_string());
                params.insert("reasoning".into(), json!({ "effort": off_value }));
            }
        }
    }
    Value::Object(params)
}

pub fn stream_azure_openai_responses(
    model: &Model,
    context: &Context,
    options: Option<&AzureOpenAIResponsesOptions>,
) -> AssistantMessageEventStream {
    let options = options.cloned();
    let model = model.clone();
    let context = context.clone();
    let (writer, reader) = create_assistant_message_event_stream();

    tokio::spawn(async move {
        let mut output =
            initial_assistant_message(API_AZURE_OPENAI_RESPONSES, &model.provider, &model.id);
        let result = run_stream(&model, &context, options.as_ref(), &mut output, &writer).await;
        end_stream(
            &model,
            &writer,
            output,
            result,
            format_stream_failure_message,
        );
    });

    reader
}

// Long by design: mirrors the provider's stream shape.
#[allow(clippy::too_many_lines)]
async fn run_stream(
    model: &Model,
    context: &Context,
    options: Option<&AzureOpenAIResponsesOptions>,
    output: &mut AssistantMessage,
    writer: &AssistantMessageEventWriter,
) -> Result<(), ProviderError> {
    let options = options.cloned().unwrap_or_default();
    let deployment_name = resolve_deployment_name(model, Some(&options));

    let api_key = options
        .base
        .api_key
        .clone()
        .or_else(|| get_env_api_key(&model.provider))
        .unwrap_or_default();
    let url = resolve_responses_url(model, Some(&options)).map_err(ProviderError::Message)?;

    let mut params = build_params(model, context, &options, &deployment_name);
    if let Some(on_payload) = &options.base.on_payload
        && let Some(next) = on_payload(params.clone(), model)
    {
        params = next;
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in model.headers.iter().flatten() {
        headers.push((name.clone(), value.clone()));
    }
    if let Some(options_headers) = &options.base.headers {
        for (name, value) in options_headers {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            headers.push((name.clone(), value.clone()));
        }
    }
    headers.push(("api-key".into(), api_key));

    crate::utils_inner::headers::ensure_json_content_type(&mut headers);
    let mut response: HttpResponse = send(RequestOptions {
        method: reqwest::Method::POST,
        url,
        headers,
        body: Some(params.to_string()),
        signal: options.base.signal.clone(),
        timeout_ms: options.base.timeout_ms,
        stall_timeout_ms: options.base.stream_stall_timeout_ms,
        connection: crate::utils_inner::stream_failure::ConnectionErrorProfile::Sdk,
        transport: crate::utils_inner::http::Transport::Http1,
    })
    .await?;

    if let Some(on_response) = &options.base.on_response {
        on_response(
            crate::types::ProviderResponse {
                status: response.status,
                headers: response.headers.clone().into_iter().collect(),
            },
            model,
        );
    }
    let request_id = response.headers.get("x-request-id").cloned();

    if response.status >= 400 {
        let body = response.read_all_text().await.unwrap_or_default();
        return Err(ProviderError::from_http_status_body(
            response.status,
            &body,
            response.headers.clone(),
        ));
    }

    writer.push(AssistantMessageEvent::Start {
        partial: output.clone(),
    });

    process_responses_sse(
        model,
        output,
        writer,
        ResponsesStreamHooks::default(),
        &mut response,
        options.base.signal.as_ref(),
        request_id.as_deref(),
    )
    .await?;

    Ok(())
}

pub fn stream_simple_azure_openai_responses(
    model: &Model,
    context: &Context,
    options: Option<&SimpleStreamOptions>,
) -> AssistantMessageEventStream {
    let api_key = options
        .and_then(|options| options.base.api_key.clone())
        .or_else(|| get_env_api_key(&model.provider));
    let Some(api_key) = api_key else {
        let (writer, reader) = create_assistant_message_event_stream();
        let message = AssistantMessage {
            content: Vec::new(),
            api: API_AZURE_OPENAI_RESPONSES.to_string(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            stop_reason_raw: None,
            error_message: Some(format!("No API key for provider: {}", model.provider)),
            timestamp: now_ms(),
            rest: Map::default(),
            discarded_usage: None,
        };
        writer.push(AssistantMessageEvent::Error {
            reason: crate::types::ErrorStopReason::Error,
            error: message.clone(),
        });
        writer.end(Some(message));
        return reader;
    };
    let base = build_base_options(model, options, Some(&api_key));
    let clamped_reasoning = options
        .and_then(|options| options.reasoning)
        .map(|reasoning| clamp_thinking_level(model, reasoning));
    let reasoning_effort = clamped_reasoning.filter(|level| *level != ModelThinkingLevel::Off);
    let stream_options = AzureOpenAIResponsesOptions {
        base,
        reasoning_effort,
        reasoning_summary: None,
        azure_api_version: None,
        azure_resource_name: None,
        azure_base_url: None,
        azure_deployment_name: None,
    };
    stream_azure_openai_responses(model, context, Some(&stream_options))
}

/// Registry provider for the `azure-openai-responses` API.
pub struct AzureOpenAIResponsesProvider;

impl Provider for AzureOpenAIResponsesProvider {
    fn api(&self) -> &str {
        API_AZURE_OPENAI_RESPONSES
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> AssistantMessageEventStream {
        let options = options.map(|base| AzureOpenAIResponsesOptions::from_base(base.clone()));
        stream_azure_openai_responses(model, context, options.as_ref())
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&SimpleStreamOptions>,
    ) -> AssistantMessageEventStream {
        stream_simple_azure_openai_responses(model, context, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url_for(base_url: Option<&str>, resource: Option<&str>, api_version: &str) -> String {
        let model = serde_json::from_value::<Model>(json!({
            "id": "gpt-5.6-terra", "name": "GPT-5.6 Terra",
            "api": "azure-openai-responses", "provider": "azure-openai-responses",
            "baseUrl": "", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 128_000, "maxTokens": 16_384
        }))
        .unwrap();
        let mut options = AzureOpenAIResponsesOptions::from_base(StreamOptions::default());
        options.azure_base_url = base_url.map(str::to_string);
        options.azure_resource_name = resource.map(str::to_string);
        options.azure_api_version = Some(api_version.to_string());
        resolve_responses_url(&model, Some(&options)).unwrap()
    }

    /// Discussion #3712: the v1 API posts to `<resource>/openai/v1/responses?api-version=v1` with
    /// the deployment in the body (byte parity with the TS `AzureOpenAI` client); the old
    /// `/deployments/<name>/responses` route 404s. An empty version falls back to `v1`, as in TS.
    #[test]
    fn v1_requests_post_to_the_responses_route() {
        let expected = "https://res.openai.azure.com/openai/v1/responses?api-version=v1";
        assert_eq!(
            [
                url_for(Some("https://res.openai.azure.com"), None, "v1"),
                url_for(Some("https://res.openai.azure.com/openai/"), None, "v1"),
                url_for(Some("https://res.openai.azure.com/openai/v1"), None, "v1"),
                url_for(None, Some("res"), "v1"),
                url_for(None, Some("res"), ""),
            ],
            [expected; 5].map(str::to_string)
        );
    }

    /// Other versions ride the same query: `preview` on the v1 path, or a dated version against a
    /// non-Azure gateway's `/openai` base.
    #[test]
    fn non_v1_versions_keep_the_api_version_query() {
        assert_eq!(
            [
                url_for(Some("https://res.openai.azure.com"), None, "preview"),
                url_for(
                    Some("https://gateway.example.com/openai"),
                    None,
                    "2025-04-01-preview"
                ),
            ],
            [
                "https://res.openai.azure.com/openai/v1/responses?api-version=preview",
                "https://gateway.example.com/openai/responses?api-version=2025-04-01-preview",
            ]
            .map(str::to_string)
        );
    }

    /// A `reasoning: false` model whose map addresses levels (the live catalog's
    /// `gpt-5.3-chat-latest` azure shape) is thinking-capable: the requested effort reaches the
    /// request with the map's value. The flag alone must not veto a route that declares addressable
    /// levels.
    #[test]
    fn a_map_addressable_model_sends_the_reasoning_effort_without_the_flag() {
        let model = serde_json::from_value::<Model>(json!({
            "id": "gpt-5.3-chat-latest", "name": "GPT-5.3 Chat (latest)",
            "api": "azure-openai-responses", "provider": "azure-openai-responses",
            "baseUrl": "", "reasoning": false,
            "thinkingLevelMap": { "off": null, "xhigh": "xhigh" }, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 128_000, "maxTokens": 16_384
        }))
        .unwrap();
        let mut options = AzureOpenAIResponsesOptions::from_base(StreamOptions::default());
        options.reasoning_effort = Some(ModelThinkingLevel::Xhigh);
        let params = build_params(&model, &Context::default(), &options, "deploy");
        assert_eq!(
            params.get("reasoning"),
            Some(&json!({ "effort": "xhigh", "summary": "auto" }))
        );
    }

    /// The request pins `store: false` (Azure stores responses server-side by default) and drops
    /// `prompt_cache_key` when the caller set cacheRetention to none; the key still ships by
    /// default.
    #[test]
    fn pins_store_false_and_gates_prompt_cache_key_on_cache_retention() {
        let model = serde_json::from_value::<Model>(json!({
            "id": "gpt-4o-mini", "name": "GPT-4o mini",
            "api": "azure-openai-responses", "provider": "azure-openai-responses",
            "baseUrl": "", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 128_000, "maxTokens": 16_384
        }))
        .unwrap();
        let mut options = AzureOpenAIResponsesOptions::from_base(StreamOptions::default());
        options.base.session_id = Some("session-1".into());

        let params = build_params(&model, &Context::default(), &options, "deploy");
        assert_eq!(params.get("store"), Some(&json!(false)));
        assert_eq!(params.get("prompt_cache_key"), Some(&json!("session-1")));

        options.base.cache_retention = Some(CacheRetention::None);
        let params = build_params(&model, &Context::default(), &options, "deploy");
        assert_eq!(params.get("store"), Some(&json!(false)));
        assert_eq!(params.get("prompt_cache_key"), None);
    }

    /// #1623 / #2089: Azure Responses streams fail closed on a missing terminal event too.
    #[tokio::test]
    async fn a_stream_without_a_terminal_event_fails_closed() {
        use crate::providers::openai_responses::tests::{
            TRUNCATED_RESPONSES_SSE,
            truncated_text_failure_message,
        };
        let server = crate::test_mock_http::serve(vec![crate::test_mock_http::MockResponse::sse(
            TRUNCATED_RESPONSES_SSE,
        )])
        .await;
        let model = serde_json::from_value::<Model>(json!({
            "id": "gpt-5", "name": "gpt-5",
            "api": "azure-openai-responses", "provider": "azure-openai-responses",
            "baseUrl": "", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 128_000, "maxTokens": 16_384
        }))
        .unwrap();
        let mut options = AzureOpenAIResponsesOptions::from_base(StreamOptions {
            api_key: Some("test".into()),
            ..Default::default()
        });
        options.azure_base_url = Some(format!("{}/openai/v1", server.base_url()));
        let message = stream_azure_openai_responses(&model, &Context::default(), Some(&options))
            .result()
            .await;
        assert_eq!(
            (message.stop_reason, message.error_message),
            (
                crate::types::StopReason::Error,
                Some(truncated_text_failure_message())
            ),
        );
    }
}
