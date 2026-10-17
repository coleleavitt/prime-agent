//! Typed agent definitions (Claude Code's `.claude/agents/*.md`): discovery
//! over the project and user directories, the parsed definition, the
//! listing a parent reads, and what a session running as one agent gets —
//! its instructions appended to the system prompt and the host-request gate
//! that enforces the part of its tool scope a kernel harness can refuse.
//!
//! `rlm.spawn(agent="name")` resolves the name here at spawn time
//! ([`discover_agents`] reads the directories fresh), fills the child's
//! model and thinking level from the definition when the call left them
//! out, and hands the definition to the child session. The child persists
//! it as one durable [`AGENT_DEFINITION_ENTRY_TYPE`] custom entry, so a
//! child revived from its session file (idle passivation, a worker
//! restart) keeps running as the same agent.

mod definition;
mod discovery;
mod policy;
mod prompt;

pub use definition::{
    AgentDefinition,
    AgentIsolation,
    AgentSource,
    is_valid_agent_name,
    parse_agent_definition,
};
pub use discovery::{AgentCatalog, AgentSearch, PROJECT_AGENT_DIRS, discover_agents};
pub use policy::{GatedTool, agent_tool_gate, allows, tools_summary};
pub use prompt::{AGENT_LISTING_HEADING, agent_instructions, agent_listing, agent_listing_line};

/// The durable custom entry a session running as an agent carries: its
/// `data` is the serialized [`AgentDefinition`].
pub const AGENT_DEFINITION_ENTRY_TYPE: &str = "agent_definition";

/// The newest [`AGENT_DEFINITION_ENTRY_TYPE`] entry in a session file's
/// lines; `None` when the session runs as no agent (or the file is
/// unreadable).
#[must_use]
pub fn agent_definition_in_session_file(session_file: &std::path::Path) -> Option<AgentDefinition> {
    let content = std::fs::read_to_string(session_file).ok()?;
    content
        .lines()
        .rev()
        .filter(|line| line.contains(AGENT_DEFINITION_ENTRY_TYPE))
        .find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            if value.get("type").and_then(serde_json::Value::as_str) != Some("custom")
                || value.get("customType").and_then(serde_json::Value::as_str)
                    != Some(AGENT_DEFINITION_ENTRY_TYPE)
            {
                return None;
            }
            serde_json::from_value(value.get("data")?.clone()).ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_definition_entry_wins_and_other_rows_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.jsonl");
        let definition = |name: &str| {
            parse_agent_definition(
                &format!("---\nname: {name}\ndescription: d\n---\nbody"),
                std::path::Path::new("a.md"),
                AgentSource::User,
            )
            .unwrap()
        };
        let row = |name: &str| {
            serde_json::json!({
                "type": "custom",
                "customType": AGENT_DEFINITION_ENTRY_TYPE,
                "data": definition(name),
            })
            .to_string()
        };
        let lines = [
            r#"{"type":"session","id":"s","timestamp":"t","cwd":"/"}"#.to_string(),
            row("first"),
            row("second"),
            r#"{"type":"message","message":{"role":"user","content":"agent_definition"}}"#
                .to_string(),
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();
        assert_eq!(
            agent_definition_in_session_file(&file),
            Some(definition("second"))
        );
        assert_eq!(
            agent_definition_in_session_file(&dir.path().join("missing")),
            None
        );
    }
}
