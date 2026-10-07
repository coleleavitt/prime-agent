//! The invariant battery: the compiled fallback catalog holds every rule
//! (the standing guard for its hand edits), each rule fails loudly on a
//! violating row, and per-transport `off` variance alone is accepted.

use super::*;

fn row(provider: &str, id: &str, api: &str, overrides: &serde_json::Value) -> Model {
    let mut value = serde_json::json!({
        "id": id,
        "name": id,
        "api": api,
        "provider": provider,
        "baseUrl": "https://example.invalid/v1",
        "reasoning": false,
        "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 200_000,
        "maxTokens": 32_000,
    });
    if let (Some(target), Some(source)) = (value.as_object_mut(), overrides.as_object()) {
        for (key, field) in source {
            target.insert(key.clone(), field.clone());
        }
    }
    serde_json::from_value(value).expect("a catalog row")
}

fn compiled_catalog() -> Vec<&'static Model> {
    crate::models_generated::get_providers()
        .into_iter()
        .flat_map(crate::models_generated::get_models)
        .collect()
}

/// The compiled catalog holds every invariant but one documented class: the
/// Prime Inference GLM rows are templates the live catalog's
/// `supported_parameters` rebuild completes (their compat turns effort on per
/// route), so their effort levels are dead only until that rebuild.
#[test]
fn the_compiled_catalog_holds_every_invariant() {
    let templates: Vec<String> = [
        ("z-ai/glm-4.5", "high"),
        ("z-ai/glm-4.5-air", "high"),
        ("z-ai/glm-4.6", "high"),
        ("z-ai/glm-4.7", "high"),
        ("z-ai/glm-4.7-flash", "high"),
        ("z-ai/glm-5", "high"),
        ("z-ai/glm-5.1", "high"),
        ("z-ai/glm-5.2", "high,xhigh"),
        ("z-ai/glm-5.3", "low,high,max"),
        ("z-ai/glm-5.3-flash", "low,high,max"),
    ]
    .iter()
    .map(|(id, levels)| {
        format!(
            "prime-inference/{id}: thinkingLevelMap offers [{levels}] but the transport cannot send reasoning effort"
        )
    })
    .collect();
    assert_eq!(validate_model_catalog(compiled_catalog()), templates);
}

#[test]
fn max_tokens_past_the_context_window_fails() {
    let models = [row(
        "huggingface",
        "thinkingmachines/Inkling-Small",
        "openai-completions",
        &serde_json::json!({ "contextWindow": 524_288, "maxTokens": 1_048_576 }),
    )];
    assert_eq!(
        validate_model_catalog(&models),
        ["huggingface/thinkingmachines/Inkling-Small: maxTokens 1048576 exceeds contextWindow 524288"]
    );
}

#[test]
fn copilot_rows_must_match_a_classified_family() {
    let models = [
        row(
            "github-copilot",
            "grok-4.6",
            "openai-completions",
            &serde_json::json!({}),
        ),
        row(
            "github-copilot",
            "llama-9",
            "openai-completions",
            &serde_json::json!({}),
        ),
        row(
            "github-copilot",
            "claude-sonnet-5",
            "anthropic-messages",
            &serde_json::json!({}),
        ),
    ];
    assert_eq!(
        validate_model_catalog(&models),
        [
            "github-copilot/grok-4.6: api openai-completions does not match classification openai-responses",
            "github-copilot/llama-9: unclassified model family; add it to copilot_model_api",
        ]
    );
}

#[test]
fn a_codex_window_diverging_past_2x_fails_unless_verified() {
    let window = |window: u64| serde_json::json!({ "contextWindow": window });
    let models = [
        row(
            "openai",
            "gpt-5.6-sol",
            "openai-responses",
            &window(1_050_000),
        ),
        row(
            "openai-codex",
            "gpt-5.6-sol",
            "openai-codex-responses",
            &window(272_000),
        ),
        row(
            "openai",
            "gpt-6-astra",
            "openai-responses",
            &window(1_050_000),
        ),
        row(
            "openai-codex",
            "gpt-6-astra",
            "openai-codex-responses",
            &window(272_000),
        ),
        row("openai", "gpt-5.5", "openai-responses", &window(400_000)),
        row(
            "openai-codex",
            "gpt-5.5",
            "openai-codex-responses",
            &window(272_000),
        ),
    ];
    assert_eq!(
        validate_model_catalog(&models),
        ["openai-codex/gpt-5.6-sol: contextWindow 272000 diverges more than 2x from openai/gpt-5.6-sol (1050000)"]
    );
}

#[test]
fn thinking_levels_must_agree_and_be_sendable() {
    let thinking =
        |map: serde_json::Value| serde_json::json!({ "reasoning": true, "thinkingLevelMap": map });
    let models = [
        // One family on one api, two providers, disagreeing on `high`.
        row(
            "opencode",
            "kimi-k3",
            "openai-completions",
            &thinking(serde_json::json!({ "low": "low", "high": "high" })),
        ),
        row(
            "huggingface",
            "moonshotai/Kimi-K3",
            "openai-completions",
            &thinking(serde_json::json!({ "low": "low", "high": null })),
        ),
        // Moonshot cannot send reasoning effort: any offered level is dead.
        row(
            "moonshotai",
            "kimi-k3-direct",
            "openai-completions",
            &thinking(serde_json::json!({ "low": "low" })),
        ),
        // A non-adaptive anthropic row offering a budget-clamped level.
        row(
            "vercel",
            "openai/gpt-5.5",
            "anthropic-messages",
            &thinking(serde_json::json!({ "xhigh": "xhigh" })),
        ),
    ];
    assert_eq!(
        validate_model_catalog(&models),
        [
            "moonshotai/kimi-k3-direct: thinkingLevelMap offers [minimal,low,medium,high] but the transport cannot send reasoning effort",
            "vercel/openai/gpt-5.5: thinkingLevelMap offers [xhigh] but the budget path serializes them as high",
            "kimi-k3 [openai-completions]: selectable thinking levels disagree across providers: huggingface/moonshotai/Kimi-K3=[minimal,low,medium], opencode/kimi-k3=[minimal,low,medium,high]",
        ]
    );
}

#[test]
fn off_level_variance_alone_is_accepted() {
    let thinking =
        |map: serde_json::Value| serde_json::json!({ "reasoning": true, "thinkingLevelMap": map });
    let models = [
        row(
            "openai",
            "gpt-5.5",
            "openai-responses",
            &thinking(serde_json::json!({ "off": "none" })),
        ),
        row(
            "azure",
            "gpt-5.5",
            "openai-responses",
            &thinking(serde_json::json!({ "off": null })),
        ),
    ];
    assert_eq!(validate_model_catalog(&models), Vec::<String>::new());
}
