//! What an agent definition's `tools` / `disallowedTools` can enforce in a
//! kernel harness, and the host-request gate that enforces it.
//!
//! The model's one tool is the Python REPL, so Claude Code's tool names map
//! onto kernel capabilities. Three are host requests the harness can refuse:
//!
//! | Claude Code tool | kernel capability | refused host requests |
//! | --- | --- | --- |
//! | `Bash` | `bash()` | `bash.run` |
//! | `Agent` / `Task` | `rlm.spawn`, `rlm.create_session` | `rlm.run`, `rlm.create_session` |
//! | `SendMessage` | `agent_message.send`, `rlm.send_message` | `agent_message.send` |
//!
//! Every other name (`Read`, `Write`, `Edit`, `Glob`, `Grep`, `WebSearch`,
//! `WebFetch`, MCP tools) is advisory: the agent's instructions state the
//! scope, but Python can read and write files and reach the network
//! directly, so the harness cannot refuse them. A pattern-scoped entry
//! (`Bash(git:*)`) allows its tool in `tools` and is advisory in
//! `disallowedTools` (the harness does not match command patterns).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use super::definition::AgentDefinition;
use crate::kernel::shared::{HostRequestGate, HostRequestPolicy};

/// A capability the harness can refuse an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatedTool {
    Bash,
    Agent,
    SendMessage,
}

impl GatedTool {
    pub const ALL: [Self; 3] = [Self::Bash, Self::Agent, Self::SendMessage];

    /// The Claude Code tool names that select this capability.
    #[must_use]
    pub fn names(self) -> &'static [&'static str] {
        match self {
            Self::Bash => &["Bash"],
            Self::Agent => &["Agent", "Task"],
            Self::SendMessage => &["SendMessage"],
        }
    }

    /// The host requests the capability is reached through.
    #[must_use]
    pub fn host_requests(self) -> &'static [&'static str] {
        match self {
            Self::Bash => &["bash.run"],
            Self::Agent => &["rlm.run", "rlm.create_session"],
            Self::SendMessage => &["agent_message.send"],
        }
    }

    /// What the refused agent loses, for the refusal message.
    fn capability(self) -> &'static str {
        match self {
            Self::Bash => "bash()",
            Self::Agent => "spawning subagents",
            Self::SendMessage => "sending agent messages",
        }
    }
}

/// `Bash(git:*)` names `Bash`; a bare name names itself.
fn base_name(entry: &str) -> &str {
    entry.split_once('(').map_or(entry, |(name, _)| name).trim()
}

/// Whether the definition's tool scope allows `tool`.
#[must_use]
pub fn allows(definition: &AgentDefinition, tool: GatedTool) -> bool {
    let listed = |entries: &[String], exact: bool| {
        entries.iter().any(|entry| {
            let entry = entry.trim();
            let name = if exact { entry } else { base_name(entry) };
            tool.names().contains(&name)
        })
    };
    let allowed = definition.tools.as_ref().is_none_or(|tools| {
        tools.iter().any(|entry| entry.trim() == "*") || listed(tools, /*exact*/ false)
    });
    allowed && !listed(&definition.disallowed_tools, /*exact*/ true)
}

/// Claude Code's tool line for the listing: `All tools`, `All tools except
/// X, Y`, the allowed list minus the denied names, or `None`.
#[must_use]
pub fn tools_summary(definition: &AgentDefinition) -> String {
    let denied = &definition.disallowed_tools;
    match &definition.tools {
        Some(tools) if !tools.iter().any(|tool| tool.trim() == "*") => {
            let kept: Vec<&str> = tools
                .iter()
                .map(String::as_str)
                .filter(|tool| !denied.iter().any(|deny| deny == tool))
                .collect();
            if kept.is_empty() {
                "None".to_string()
            } else {
                kept.join(", ")
            }
        }
        Some(_) | None if denied.is_empty() => "All tools".to_string(),
        Some(_) | None => format!("All tools except {}", denied.join(", ")),
    }
}

/// The host-request gate for a session running as `definition`: refuses
/// the requests of every capability the definition denies, then defers to
/// `inner` (the installed features' gate). `None` when the definition
/// denies nothing the harness can refuse, so `inner` stays as it was.
#[must_use]
pub fn agent_tool_gate(
    definition: &AgentDefinition,
    inner: Option<HostRequestGate>,
) -> Option<HostRequestGate> {
    let denied: Vec<GatedTool> = GatedTool::ALL
        .into_iter()
        .filter(|tool| !allows(definition, *tool))
        .collect();
    if denied.is_empty() {
        return None;
    }
    Some(Arc::new(AgentToolGate {
        agent: definition.name.clone(),
        denied,
        inner,
    }))
}

struct AgentToolGate {
    agent: String,
    denied: Vec<GatedTool>,
    inner: Option<HostRequestGate>,
}

type GateFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

impl HostRequestPolicy for AgentToolGate {
    fn review(&self, request_type: &str, data: &Value) -> GateFuture<Option<String>> {
        if let Some(tool) = self
            .denied
            .iter()
            .find(|tool| tool.host_requests().contains(&request_type))
        {
            let reason = format!(
                "This agent (\"{}\") is not allowed {}: its definition does not grant {}",
                self.agent,
                tool.capability(),
                tool.names()[0]
            );
            return Box::pin(async move { Some(reason) });
        }
        match &self.inner {
            Some(inner) => inner.review(request_type, data),
            None => Box::pin(async { None }),
        }
    }

    fn after(
        &self,
        request_type: &str,
        data: &Value,
        outcome: Result<&Value, &str>,
    ) -> GateFuture<()> {
        match &self.inner {
            Some(inner) => inner.after(request_type, data, outcome),
            None => Box::pin(async {}),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::agents::definition::AgentSource;

    fn definition(tools: Option<&[&str]>, denied: &[&str]) -> AgentDefinition {
        AgentDefinition {
            name: "scoped".into(),
            description: "d".into(),
            prompt: String::new(),
            tools: tools.map(|tools| tools.iter().map(ToString::to_string).collect()),
            disallowed_tools: denied.iter().map(ToString::to_string).collect(),
            model: None,
            effort: None,
            color: None,
            isolation: None,
            memory: None,
            skills: Vec::new(),
            max_turns: None,
            background: false,
            initial_prompt: None,
            source: AgentSource::Project,
            path: PathBuf::new(),
        }
    }

    fn allowed(definition: &AgentDefinition) -> [bool; 3] {
        GatedTool::ALL.map(|tool| allows(definition, tool))
    }

    #[test]
    fn scopes_map_onto_the_gated_capabilities() {
        assert_eq!(allowed(&definition(None, &[])), [true, true, true]);
        assert_eq!(
            allowed(&definition(Some(&["Read", "Bash(git:*)"]), &[])),
            [true, false, false]
        );
        assert_eq!(
            allowed(&definition(Some(&["*"]), &["Task", "Bash(rm:*)"])),
            [true, false, true]
        );
        assert_eq!(
            allowed(&definition(None, &["SendMessage", "Bash"])),
            [false, true, false]
        );
    }

    #[test]
    fn the_listing_names_the_tools_like_claude_code() {
        assert_eq!(
            [
                tools_summary(&definition(None, &[])),
                tools_summary(&definition(None, &["Bash", "Agent"])),
                tools_summary(&definition(Some(&["Read", "Bash"]), &["Bash"])),
                tools_summary(&definition(Some(&["Bash"]), &["Bash"])),
            ],
            [
                "All tools".to_string(),
                "All tools except Bash, Agent".to_string(),
                "Read".to_string(),
                "None".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn the_gate_refuses_denied_requests_and_defers_the_rest() {
        assert!(agent_tool_gate(&definition(None, &[]), None).is_none());
        let gate = agent_tool_gate(&definition(Some(&["Read"]), &[]), None).unwrap();
        let verdicts = [
            gate.review("bash.run", &Value::Null).await,
            gate.review("rlm.run", &Value::Null).await,
            gate.review("rlm.collect", &Value::Null).await,
        ];
        assert_eq!(
            verdicts,
            [
                Some(
                    "This agent (\"scoped\") is not allowed bash(): its definition does not grant Bash"
                        .to_string()
                ),
                Some(
                    "This agent (\"scoped\") is not allowed spawning subagents: its definition does not grant Agent"
                        .to_string()
                ),
                None,
            ]
        );
    }
}
