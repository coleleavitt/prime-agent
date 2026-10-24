//! The instruction-size notice (`instruction_size_warning`): Claude Code's
//! large-instruction-file diagnostics ([`crate::resources::instructions::size`])
//! as a transcript row at session start. The row is for the user: the model
//! never receives it (`convert_to_llm` drops the type).

use pa_types::session::CustomMessage;

pub const INSTRUCTION_SIZE_WARNING_CUSTOM_TYPE: &str = "instruction_size_warning";

/// The notice for `warnings`; `None` when there are none.
#[must_use]
pub fn notice_message(warnings: &[String]) -> Option<CustomMessage> {
    if warnings.is_empty() {
        return None;
    }
    let list = warnings
        .iter()
        .map(|warning| format!("- {warning}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(CustomMessage {
        custom_type: INSTRUCTION_SIZE_WARNING_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(format!(
            "[instruction-size-warning]\n\n{list}\n\nEvery request carries these files; trim them or move details into files the agent reads when needed."
        )),
        display: true,
        details: Some(serde_json::json!({ "warnings": warnings })),
        timestamp: super::refine::now_millis(),
        rest: serde_json::Map::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_warnings_no_row() {
        assert_eq!(notice_message(&[]), None);
    }

    #[test]
    fn the_row_lists_each_warning_and_never_reaches_the_model() {
        let row = notice_message(&["Large /r/CLAUDE.md will impact performance (45.0k chars > 40.0k)".to_string()])
            .expect("a row");
        assert_eq!(row.custom_type, INSTRUCTION_SIZE_WARNING_CUSTOM_TYPE);
        assert!(row.display);
        assert!(row
            .content
            .text()
            .contains("- Large /r/CLAUDE.md will impact performance (45.0k chars > 40.0k)"));
        let converted = super::super::messages::convert_to_llm(&[
            pa_types::session::AgentMessage::Custom(row),
        ]);
        assert!(converted.is_empty());
    }
}
