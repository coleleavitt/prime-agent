//! pi's whole request for representative conversations, against the
//! plugin's own output (`tests/fixtures/golden/pi_requests.json`, recorded
//! by `generate_requests.ts` from pi's provider entry point).

use std::collections::BTreeMap;

use anthropic::claude_code::{stainless_arch, stainless_os};
use pa_ai::request_hooks::{CallerOptions, RequestSource};
use pa_ai::types::{Context, ModelThinkingLevel, ThinkingBudgets};
use serde_json::Value;

use super::*;
use crate::pi::settings::request_settings;
use crate::shape::{claude_code_headers, ShapeEnv};

pub(crate) fn golden() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/golden/pi_requests.json"
    ))
    .expect("the golden parses")
}

/// A golden case's conversation, options, settings and identity.
pub(crate) struct Case {
    pub(crate) name: String,
    pub(crate) model: String,
    pub(crate) context: Context,
    pub(crate) options: CallerOptions,
    pub(crate) settings: RequestSettings,
    pub(crate) identity: ShapeIdentity,
    pub(crate) token: String,
    pub(crate) body_text: String,
    pub(crate) headers: BTreeMap<String, String>,
}

impl Case {
    pub(crate) fn source(&self) -> RequestSource<'_> {
        RequestSource {
            context: &self.context,
            options: &self.options,
            session_id: None,
        }
    }
}

pub(crate) fn cases() -> Vec<Case> {
    golden()["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .map(|case| {
            let options = &case["options"];
            let mut headers: BTreeMap<String, String> = case["headers"]
                .as_object()
                .expect("headers")
                .iter()
                .map(|(name, value)| (name.clone(), value.as_str().unwrap_or_default().to_string()))
                .collect();
            // The plugin maps process.platform/arch as these map Rust's.
            headers.insert("x-stainless-os".to_string(), stainless_os().to_string());
            headers.insert("x-stainless-arch".to_string(), stainless_arch().to_string());
            Case {
                name: case["name"].as_str().expect("a name").to_string(),
                model: case["model"].as_str().expect("a model").to_string(),
                context: serde_json::from_value(case["context"].clone()).expect("a context"),
                options: CallerOptions {
                    reasoning: serde_json::from_value::<Option<ModelThinkingLevel>>(
                        options.get("reasoning").cloned().unwrap_or(Value::Null),
                    )
                    .expect("a reasoning level"),
                    thinking_budgets: options.get("thinkingBudgets").map(|budgets| {
                        serde_json::from_value::<ThinkingBudgets>(budgets.clone()).expect("budgets")
                    }),
                    max_tokens: options.get("maxTokens").and_then(Value::as_u64),
                },
                settings: request_settings(case["settings"].as_object().expect("settings")),
                identity: ShapeIdentity {
                    device_id: case["identity"]["deviceId"].as_str().map(str::to_string),
                    account_uuid: case["identity"]["accountUuid"].as_str().map(str::to_string),
                    session_id: case["identity"]["sessionId"]
                        .as_str()
                        .expect("a session id")
                        .to_string(),
                },
                token: case["token"].as_str().expect("a token").to_string(),
                body_text: case["bodyText"].as_str().expect("a body").to_string(),
                headers,
            }
        })
        .collect()
}

pub(crate) fn version() -> String {
    golden()["version"].as_str().expect("a version").to_string()
}

#[test]
fn every_conversation_is_built_byte_for_byte_as_pi_builds_it() {
    let version = version();
    for case in cases() {
        let built = build_request(
            &case.model,
            &case.source(),
            case.settings,
            &case.identity,
            &version,
            None,
        );
        assert_eq!(built.body_text, case.body_text, "{}", case.name);
    }
}

#[test]
fn every_request_carries_pi_s_headers() {
    let version = version();
    for case in cases() {
        let built = build_request(
            &case.model,
            &case.source(),
            case.settings,
            &case.identity,
            &version,
            None,
        );
        let headers: BTreeMap<String, String> = claude_code_headers(
            &case.token,
            &built.body,
            &case.identity,
            &version,
            &ShapeEnv::default(),
            "",
            "request-id",
        )
        .into_iter()
        .filter(|(name, _)| name != "x-client-request-id")
        .collect();
        assert_eq!(headers, case.headers, "{}", case.name);
    }
}

#[test]
fn the_system_prompt_splits_on_pi_s_documentation_paragraph() {
    assert_eq!(
        split_system_prompt("Identity.\n\n\nPi documentation: here\n\nRules."),
        (
            Some("Identity.\n\nRules.".to_string()),
            "Pi documentation: here".to_string()
        )
    );
    assert_eq!(
        split_system_prompt("An unknown\n\nprompt."),
        (None, "An unknown\n\nprompt.".to_string())
    );
    assert_eq!(
        split_system_prompt("Pi documentation only"),
        (None, "Pi documentation only".to_string())
    );
}

#[test]
fn tool_ids_are_sanitized_per_utf16_unit() {
    assert_eq!(sanitize_tool_id(""), "tool_call_unknown");
    assert_eq!(sanitize_tool_id("call_1|fc 1"), "call_1_fc_1");
    assert_eq!(sanitize_tool_id("a\u{1F600}b"), "a__b");
    assert_eq!(sanitize_tool_id(&"x".repeat(300)).len(), 256);
}

#[test]
fn openai_reasoning_signatures_are_recognized() {
    assert!(is_openai_reasoning_signature(Some("gAAAAB")));
    assert!(is_openai_reasoning_signature(Some(
        r#"{"type":"reasoning","id":"rs_1","encrypted_content":"gAAAAx"}"#
    )));
    assert!(!is_openai_reasoning_signature(Some(
        r#"{"type":"reasoning"}"#
    )));
    assert!(!is_openai_reasoning_signature(Some("EqQB")));
    assert!(!is_openai_reasoning_signature(None));
}
