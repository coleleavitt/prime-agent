//! Dropped-tool-call recovery (upstream #2530, TS `tool-intent-recovery.ts`):
//! the eligibility rule for a reply that reported a tool call and delivered
//! none, and the hidden model-facing row the one retry turn carries. The
//! agent loop owns the once-per-run bound and the tool-choice override; the
//! embedding that owns the queue/goal/autonomous policy installs the hook.

/// The custom type of the hidden recovery row.
pub const TOOL_INTENT_RECOVERY_CUSTOM_TYPE: &str = "tool_intent_recovery";

/// The recovery row's model-facing text (TS `createToolIntentRecoveryMessage`).
pub const TOOL_INTENT_RECOVERY_TEXT: &str = "Your previous reply ended before it was complete. If you were about to call a tool, call it now within the user's existing instructions and permissions; otherwise finish the reply. Do not repeat the preamble.";

/// The hidden (`display: false`) recovery row the retry turn delivers.
///
/// # Panics
///
/// Never in practice: a plain custom message always serializes.
#[must_use]
pub fn tool_intent_recovery_row(timestamp: u64) -> pa_agent::types::AgentMessage {
    let custom = pa_types::session::CustomMessage {
        custom_type: TOOL_INTENT_RECOVERY_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(TOOL_INTENT_RECOVERY_TEXT.to_string()),
        display: false,
        details: None,
        timestamp,
        rest: serde_json::Map::default(),
    };
    pa_agent::types::AgentMessage::Custom(pa_agent::types::CustomAgentMessage {
        role: "custom".to_string(),
        payload: serde_json::to_value(&custom).expect("recovery row payload serializes"),
    })
}

/// Whether `message` is a finish eligible for the one retry (TS
/// `isDroppedToolCallStop`): no delivered tool call, and either `toolUse`
/// (any model) or `length` on a model whose catalog compat sets
/// `retryOnTruncatedToolCall`. A plain `stop` never qualifies.
#[must_use]
pub fn is_dropped_tool_call_stop(
    message: &pa_agent::types::AssistantMessage,
    model: Option<&pa_types::ai::Model>,
) -> bool {
    if !message.tool_calls().is_empty() {
        return false;
    }
    match message.stop_reason {
        pa_agent::types::StopReason::ToolUse => true,
        pa_agent::types::StopReason::Length => model.is_some_and(retries_on_truncated_tool_call),
        pa_agent::types::StopReason::Stop
        | pa_agent::types::StopReason::Error
        | pa_agent::types::StopReason::Aborted => false,
    }
}

fn retries_on_truncated_tool_call(model: &pa_types::ai::Model) -> bool {
    model.api == "openai-completions"
        && model.compat.as_ref().is_some_and(|compat| {
            compat.raw.get("retryOnTruncatedToolCall") == Some(&serde_json::Value::Bool(true))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(stop_reason: &str, with_call: bool) -> pa_agent::types::AssistantMessage {
        let mut content =
            vec![serde_json::json!({ "type": "text", "text": "Let me check the logs." })];
        if with_call {
            content.push(serde_json::json!({
                "type": "toolCall", "id": "call-1", "name": "ipython", "arguments": {}
            }));
        }
        serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": content,
            "usage": {
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 }
            },
            "stopReason": stop_reason,
            "timestamp": 0,
        }))
        .expect("assistant reply")
    }

    fn model(compat: Option<serde_json::Value>) -> pa_types::ai::Model {
        let mut model = crate::models::prime_inference::private_prime_inference_models()
            .into_iter()
            .next()
            .expect("a bundled private template");
        model.compat = compat.map(|value| serde_json::from_value(value).expect("compat"));
        model
    }

    #[test]
    fn only_undelivered_tool_use_or_flagged_length_finishes_qualify() {
        let flagged = model(Some(
            serde_json::json!({ "retryOnTruncatedToolCall": true }),
        ));
        let unflagged = model(Some(
            serde_json::json!({ "retryOnTruncatedToolCall": false }),
        ));
        let cases = [
            (reply("toolUse", false), None, true),
            (reply("toolUse", true), None, false),
            (reply("length", false), Some(&flagged), true),
            (reply("length", true), Some(&flagged), false),
            (reply("length", false), Some(&unflagged), false),
            (reply("length", false), None, false),
            (reply("stop", false), Some(&flagged), false),
            (reply("error", false), Some(&flagged), false),
            (reply("aborted", false), Some(&flagged), false),
        ];
        let verdicts = cases
            .iter()
            .map(|(message, model, _)| is_dropped_tool_call_stop(message, *model))
            .collect::<Vec<_>>();
        let expected = cases.iter().map(|(_, _, want)| *want).collect::<Vec<_>>();
        assert_eq!(verdicts, expected);
    }

    #[test]
    fn the_glm_5_3_templates_carry_the_retry_flag() {
        let public = pa_ai::models_generated::get_model("prime-inference", "z-ai/glm-5.3")
            .expect("compiled GLM 5.3");
        let private = crate::models::prime_inference::private_prime_inference_models()
            .into_iter()
            .find(|model| model.id == "internal/glm-5.3-fast")
            .expect("bundled GLM 5.3 Fast");
        let length = reply("length", false);
        assert_eq!(
            [
                is_dropped_tool_call_stop(&length, Some(public)),
                is_dropped_tool_call_stop(&length, Some(&private)),
            ],
            [true, true]
        );
    }

    #[test]
    fn the_recovery_row_is_hidden_model_context() {
        let row = tool_intent_recovery_row(7);
        let converted = super::super::messages::loop_convert_to_llm(vec![row.clone()]);
        assert_eq!(
            serde_json::to_value(&row).expect("row"),
            serde_json::json!({
                "role": "custom",
                "customType": TOOL_INTENT_RECOVERY_CUSTOM_TYPE,
                "content": TOOL_INTENT_RECOVERY_TEXT,
                "display": false,
                "timestamp": 7,
            })
        );
        assert_eq!(converted.len(), 1);
    }
}
