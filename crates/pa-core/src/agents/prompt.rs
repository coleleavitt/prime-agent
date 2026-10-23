//! The model-facing text of agent definitions: the listing a parent reads
//! (Claude Code's agent listing) and the instructions a session running as
//! one agent reads (appended to its system prompt).

use std::fmt::Write as _;

use super::definition::AgentDefinition;
use super::discovery::AgentCatalog;
use super::policy::{GatedTool, allows, tools_summary};

/// The listing's heading (Claude Code's "Available agent types for the
/// Agent tool:", for `rlm.spawn`).
pub const AGENT_LISTING_HEADING: &str = "Available agent types for `rlm.spawn(agent=...)`:";

/// One listing line: `- name: description (Tools: ...)`.
#[must_use]
pub fn agent_listing_line(definition: &AgentDefinition) -> String {
    let description = definition.description.replace('\n', " ");
    format!(
        "- {}: {} (Tools: {})",
        definition.name,
        description.trim(),
        tools_summary(definition)
    )
}

/// The listing of every discovered agent type; `None` when there is none.
#[must_use]
pub fn agent_listing(catalog: &AgentCatalog) -> Option<String> {
    if catalog.agents.is_empty() {
        return None;
    }
    let mut lines = vec![AGENT_LISTING_HEADING.to_string()];
    lines.extend(catalog.agents.iter().map(agent_listing_line));
    Some(lines.join("\n"))
}

/// The instructions a session running as `definition` reads at the end of
/// its system prompt: the definition's body, its tool scope (which of it
/// the harness enforces), and the skills to load first.
#[must_use]
pub fn agent_instructions(definition: &AgentDefinition) -> String {
    let mut text = format!("# Agent: {}\n\n", definition.name);
    if definition.prompt.trim().is_empty() {
        text.push_str(definition.description.trim());
    } else {
        text.push_str(definition.prompt.trim());
    }
    let has_scope = definition.tools.is_some() || !definition.disallowed_tools.is_empty();
    if has_scope {
        let refused: Vec<&str> = GatedTool::ALL
            .into_iter()
            .filter(|tool| !allows(definition, *tool))
            .map(|tool| tool.names()[0])
            .collect();
        let _ = write!(
            text,
            "\n\nTool scope (Claude Code tool names): {}.",
            tools_summary(definition)
        );
        if refused.is_empty() {
            text.push_str(
                " Keep to it: the harness does not refuse file, search or web access from Python.",
            );
        } else {
            let _ = write!(
                text,
                " The harness refuses {} for this agent; keep to the rest of the scope yourself.",
                refused.join(", ")
            );
        }
    }
    if !definition.skills.is_empty() {
        let _ = write!(
            text,
            "\n\nLoad these skills before starting: {}.",
            definition.skills.join(", ")
        );
    }
    if let Some(turns) = definition.max_turns {
        let _ = write!(
            text,
            "\n\nFinish within about {turns} turns; report what is left if you cannot."
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agents::definition::AgentSource;

    fn definition(name: &str, tools: Option<Vec<String>>) -> AgentDefinition {
        AgentDefinition {
            name: name.into(),
            description: "Finds\nthings".into(),
            prompt: "You search.".into(),
            tools,
            disallowed_tools: Vec::new(),
            model: None,
            effort: None,
            color: None,
            isolation: None,
            memory: None,
            skills: vec!["tasks".into()],
            max_turns: Some(5),
            background: false,
            initial_prompt: None,
            source: AgentSource::Project,
            path: PathBuf::new(),
        }
    }

    #[test]
    fn the_listing_is_one_line_per_type() {
        let catalog = AgentCatalog {
            agents: vec![
                definition("explore", Some(vec!["Read".into(), "Grep".into()])),
                definition("general", None),
            ],
            diagnostics: Vec::new(),
        };
        assert_eq!(
            agent_listing(&catalog).unwrap(),
            "Available agent types for `rlm.spawn(agent=...)`:\n\
             - explore: Finds things (Tools: Read, Grep)\n\
             - general: Finds things (Tools: All tools)"
        );
        assert_eq!(agent_listing(&AgentCatalog::default()), None);
    }

    #[test]
    fn instructions_state_the_enforced_scope() {
        let text = agent_instructions(&definition("explore", Some(vec!["Read".into()])));
        assert_eq!(
            text,
            "# Agent: explore\n\nYou search.\n\n\
             Tool scope (Claude Code tool names): Read. The harness refuses Bash, Agent, SendMessage \
             for this agent; keep to the rest of the scope yourself.\n\n\
             Load these skills before starting: tasks.\n\n\
             Finish within about 5 turns; report what is left if you cannot."
        );
    }
}
