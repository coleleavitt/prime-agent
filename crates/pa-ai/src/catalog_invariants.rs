//! Model catalog invariants (upstream #2042): rules a generated catalog must
//! hold, checked over whole catalogs so a violating regeneration fails by name
//! instead of shipping rows that surface as provider 400s, early compaction,
//! or dead `/effort` levels. The checks reuse the runtime's own rules
//! (thinking-level support, adaptive thinking, the completions compat), so
//! they never re-derive what a request actually sends.
//!
//! The compiled fallback catalog (`models_generated`) and the live catalog
//! payload (`pa-models`) are both checked against these.

use std::collections::BTreeMap;

use pa_types::ai::thinking_levels::get_supported_thinking_levels;
use pa_types::ai::{Model, ModelThinkingLevel};

/// The one endpoint GitHub Copilot serves each model family through; `None`
/// for an unclassified family, which fails validation by name rather than
/// silently defaulting to chat completions.
#[must_use]
pub fn copilot_model_api(model_id: &str) -> Option<&'static str> {
    if model_id.starts_with("claude-") {
        return Some("anthropic-messages");
    }
    // Responses-only on Copilot: /chat/completions rejects these families.
    if ["gpt-5", "gpt-6", "oswe", "grok-", "mai-"]
        .iter()
        .any(|prefix| model_id.starts_with(prefix))
    {
        return Some("openai-responses");
    }
    if model_id.starts_with("gemini-") || model_id.starts_with("kimi-") {
        return Some("openai-completions");
    }
    None
}

/// Codex rows whose ChatGPT-backend window is verified smaller than the API
/// side (Codex CLI models.json): exempt from the 2x window rule.
pub const CODEX_SMALLER_WINDOW_VERIFIED: &[&str] = &["gpt-6-astra"];

/// The model family across providers: the id's last path segment, lowercased.
fn family_key(model_id: &str) -> String {
    model_id
        .rsplit('/')
        .next()
        .unwrap_or(model_id)
        .to_lowercase()
}

/// The runtime-selectable levels, modulo `off` (whether thinking can be
/// disabled legitimately varies per transport).
fn selectable_levels(model: &Model) -> String {
    get_supported_thinking_levels(model)
        .into_iter()
        .filter(|level| *level != ModelThinkingLevel::Off)
        .map(ModelThinkingLevel::wire_name)
        .collect::<Vec<_>>()
        .join(",")
}

/// Plain openai-format completions gate the reasoning parameter on compat;
/// the other formats use the map as an enable toggle.
fn effort_is_sendable(model: &Model) -> bool {
    let compat = crate::providers::openai_completions::get_compat(model);
    compat.thinking_format != crate::types::ThinkingFormat::Openai
        || compat.supports_reasoning_effort
}

/// Every invariant violation across `models` (one catalog, deduplicated by
/// provider and id); an empty result means the catalog is valid.
///
/// 1. `maxTokens <= contextWindow` on every row.
/// 2. Every `github-copilot` id belongs to a classified family, and its `api`
///    is that family's endpoint.
/// 3. An `openai-codex` row's window stays within 2x of the same model's
///    `openai` row (unless verified smaller).
/// 4. A non-adaptive `anthropic-messages` row offers no `xhigh`/`max` (the
///    budget path serializes them as `high`); an `openai-completions` row
///    that cannot send reasoning effort offers no level; and same-family rows
///    on one api agree on their selectable levels across providers.
// One pass per invariant, in the TS validator's order (the violation order
// is part of the regen report); splitting would only scatter that order.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn validate_model_catalog<'a>(models: impl IntoIterator<Item = &'a Model>) -> Vec<String> {
    let models: Vec<&Model> = models.into_iter().collect();
    let mut violations = Vec::new();
    for model in &models {
        if model.max_tokens > model.context_window {
            violations.push(format!(
                "{}/{}: maxTokens {} exceeds contextWindow {}",
                model.provider, model.id, model.max_tokens, model.context_window
            ));
        }
    }
    for model in models
        .iter()
        .filter(|model| model.provider == "github-copilot")
    {
        match copilot_model_api(&model.id) {
            None => violations.push(format!(
                "github-copilot/{}: unclassified model family; add it to copilot_model_api",
                model.id
            )),
            Some(expected) if model.api != expected => violations.push(format!(
                "github-copilot/{}: api {} does not match classification {expected}",
                model.id, model.api
            )),
            Some(_) => {}
        }
    }
    for model in models
        .iter()
        .filter(|model| model.provider == "openai-codex")
    {
        if CODEX_SMALLER_WINDOW_VERIFIED.contains(&model.id.as_str()) {
            continue;
        }
        let Some(twin) = models
            .iter()
            .find(|twin| twin.provider == "openai" && twin.id == model.id)
        else {
            continue;
        };
        // Integer form of `ratio > 2 || ratio < 0.5` over the two windows.
        if twin.context_window > 2 * model.context_window
            || 2 * twin.context_window < model.context_window
        {
            violations.push(format!(
                "openai-codex/{}: contextWindow {} diverges more than 2x from openai/{} ({})",
                model.id, model.context_window, model.id, twin.context_window
            ));
        }
    }
    let mut family_levels: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for model in &models {
        let Some(map) = model
            .thinking_level_map
            .as_ref()
            .filter(|_| model.reasoning)
        else {
            continue;
        };
        if model.api == "anthropic-messages"
            && !crate::providers::anthropic::supports_adaptive_thinking(&model.id)
        {
            let clamped: Vec<&str> = [ModelThinkingLevel::Xhigh, ModelThinkingLevel::Max]
                .into_iter()
                .filter(|level| map.get(level).is_some_and(Option::is_some))
                .map(ModelThinkingLevel::wire_name)
                .collect();
            if !clamped.is_empty() {
                violations.push(format!(
                    "{}/{}: thinkingLevelMap offers [{}] but the budget path serializes them as high",
                    model.provider,
                    model.id,
                    clamped.join(",")
                ));
            }
        }
        if model.api == "openai-completions" && !effort_is_sendable(model) {
            let levels = selectable_levels(model);
            if !levels.is_empty() {
                violations.push(format!(
                    "{}/{}: thinkingLevelMap offers [{levels}] but the transport cannot send reasoning effort",
                    model.provider, model.id
                ));
            }
            continue;
        }
        family_levels
            .entry(format!("{} [{}]", family_key(&model.id), model.api))
            .or_default()
            .insert(
                format!("{}/{}", model.provider, model.id),
                selectable_levels(model),
            );
    }
    for (key, seen) in family_levels {
        let mut distinct: Vec<&String> = seen.values().collect();
        distinct.sort();
        distinct.dedup();
        if distinct.len() > 1 {
            let detail = seen
                .iter()
                .map(|(row, levels)| format!("{row}=[{levels}]"))
                .collect::<Vec<_>>()
                .join(", ");
            violations.push(format!(
                "{key}: selectable thinking levels disagree across providers: {detail}"
            ));
        }
    }
    violations
}

#[cfg(test)]
mod tests;
