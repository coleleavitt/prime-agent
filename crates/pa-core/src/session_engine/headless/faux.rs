//! The faux-script verification seam's response steps
//! (`PRIME_AGENT_FAUX_SCRIPT`; never set by the product): the print
//! root's queue and every scripted inline child's `children.<name>` queue
//! parse through one parser.

/// The text of the request's user messages that are harness context rows.
fn context_reminder_rows<T: serde::Serialize>(messages: &[T]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|message| serde_json::to_value(message).ok())
        .filter(|message| message["role"] == "user")
        .flat_map(|message| {
            let content = &message["content"];
            match content {
                serde_json::Value::String(text) => vec![text.clone()],
                serde_json::Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| part["text"].as_str().map(str::to_string))
                    .collect(),
                _ => Vec::new(),
            }
        })
        .filter(|text| text.starts_with("<system-reminder>"))
        .collect()
}

/// One scripted response entry: a plain string answers with fixed text;
/// `{"systemPrompt": true}` answers with the request's system prompt and its
/// harness context rows; other objects go through the shared faux-script parser.
///
/// # Errors
///
/// Returns the faux-script parser's error for an object entry it rejects.
///
/// # Panics
///
/// Panics if the parser turns one object entry into no response step.
pub fn faux_response_steps(
    entries: &[serde_json::Value],
) -> Result<Vec<pa_ai::faux::FauxResponseStep>, String> {
    entries
        .iter()
        .map(|entry| match entry {
            serde_json::Value::String(text) => Ok(pa_ai::faux::FauxResponseStep::Message(
                pa_ai::faux::faux_assistant_text_message(
                    text,
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ),
            )),
            serde_json::Value::Object(map)
                if map.get("systemPrompt").and_then(serde_json::Value::as_bool) == Some(true) =>
            {
                Ok(pa_ai::faux::FauxResponseStep::Factory(std::sync::Arc::new(
                    |context, _options, _call, _model| {
                        // What the model is told: the system prompt, then the
                        // harness context rows (`<system-reminder>` user
                        // messages: instruction files, skills, environment).
                        let mut text = context.system_prompt.clone().unwrap_or_default();
                        for row in context_reminder_rows(&context.messages) {
                            text.push_str("\n\n");
                            text.push_str(&row);
                        }
                        Ok(pa_ai::faux::faux_assistant_text_message(
                            &text,
                            pa_ai::faux::FauxAssistantMessageOptions::default(),
                        ))
                    },
                )))
            }
            serde_json::Value::Object(_) => {
                pa_ai::faux::script::parse_faux_script(&serde_json::json!({
                    "responses": [entry]
                }))
                .map(|parsed| {
                    let mut steps = parsed.responses.into_iter();
                    let first = steps
                        .next()
                        .expect("an object entry parses into one response step");
                    debug_assert!(steps.next().is_none());
                    first
                })
            }
            _ => Ok(pa_ai::faux::FauxResponseStep::Message(
                pa_ai::faux::faux_assistant_text_message(
                    "",
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ),
            )),
        })
        .collect()
}
