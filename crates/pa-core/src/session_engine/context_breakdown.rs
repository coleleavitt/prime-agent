//! `/context`'s category breakdown (Claude Code's `analyzeContext` grid):
//! where the live context's tokens go, by category. The fixed parts are
//! estimated here from what the session sends (chars / 4, like Claude Code's
//! local fallback): the system prompt with the session's environment rows,
//! the tool definitions, the instruction files and auto memory, the skills
//! inventory, the MCP server listing and the agent types. Messages are the
//! rest of the measured context; the autocompact buffer is the window above
//! the compaction trigger, and free space what is left.

use serde_json::{Value, json};

use crate::prompts::session_context::{
    AMBIENT_CONTEXT_NOTE,
    SessionContext,
    render_date,
    render_environment,
    render_instruction_files,
    render_mcp_servers,
    render_skills,
};

/// Claude Code's category names, in its legend order.
pub const SYSTEM_PROMPT: &str = "System prompt";
pub const SYSTEM_TOOLS: &str = "System tools";
pub const MCP_SERVERS: &str = "MCP servers";
pub const CUSTOM_AGENTS: &str = "Custom agents";
pub const MEMORY_FILES: &str = "Memory files";
pub const SKILLS: &str = "Skills";
pub const MESSAGES: &str = "Messages";
pub const AUTOCOMPACT_BUFFER: &str = "Autocompact buffer";
pub const FREE_SPACE: &str = "Free space";

/// One category's estimated tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextCategory {
    pub name: &'static str,
    pub tokens: u64,
}

/// The fixed categories plus the session's own context estimate; the wire
/// form ([`ContextBreakdown::to_wire`]) folds in a measured total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBreakdown {
    /// The fixed categories, in legend order (zero-token ones included).
    pub fixed: Vec<ContextCategory>,
    pub context_window: u64,
    /// The compaction trigger, when auto-compaction is on and the window
    /// is known.
    pub compact_at: Option<u64>,
    /// The session's own estimate of the whole context (the last provider
    /// usage plus trailing messages, else chars / 4 of everything).
    pub estimated_total: u64,
    pub model: String,
}

/// Tokens for `text` (chars / 4, rounded up).
#[must_use]
pub fn estimate_text_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// What the fixed categories cost, from the session's inputs.
pub struct BreakdownInputs<'a> {
    pub system_prompt: &'a str,
    /// The auto-memory section the system prompt carries (counted as a
    /// memory file, not as system prompt).
    pub auto_memory: Option<&'a str>,
    /// Each tool definition's name, description and JSON parameters.
    pub tools: Vec<(&'a str, &'a str, &'a Value)>,
    pub context: &'a SessionContext,
}

/// The fixed categories in Claude Code's legend order.
#[must_use]
pub fn fixed_categories(inputs: &BreakdownInputs<'_>) -> Vec<ContextCategory> {
    let memory = inputs.auto_memory.map_or(0, estimate_text_tokens);
    let context = inputs.context;
    // The environment rows ride the session context but are harness text,
    // like Claude Code's environment section of the system prompt.
    let environment = estimate_text_tokens(&render_environment(&context.environment))
        + estimate_text_tokens(&render_date(&context.date))
        + estimate_text_tokens(AMBIENT_CONTEXT_NOTE);
    let system_prompt =
        estimate_text_tokens(inputs.system_prompt).saturating_sub(memory) + environment;
    let tools = inputs
        .tools
        .iter()
        .map(|(name, description, parameters)| {
            estimate_text_tokens(name)
                + estimate_text_tokens(description)
                + estimate_text_tokens(&parameters.to_string())
        })
        .sum();
    vec![
        ContextCategory {
            name: SYSTEM_PROMPT,
            tokens: system_prompt,
        },
        ContextCategory {
            name: SYSTEM_TOOLS,
            tokens: tools,
        },
        ContextCategory {
            name: MCP_SERVERS,
            tokens: estimate_text_tokens(&render_mcp_servers(&context.mcp_servers)),
        },
        ContextCategory {
            name: CUSTOM_AGENTS,
            tokens: estimate_text_tokens(&context.agent_types),
        },
        ContextCategory {
            name: MEMORY_FILES,
            tokens: estimate_text_tokens(&render_instruction_files(&context.instruction_files))
                + memory,
        },
        ContextCategory {
            name: SKILLS,
            tokens: estimate_text_tokens(&render_skills(&context.skills)),
        },
    ]
}

impl ContextBreakdown {
    /// The wire shape `get_context_tree` carries as `contextBreakdown`:
    /// every category with tokens (the fixed ones, `Messages` as the rest of
    /// `measured` or the session's estimate, the autocompact buffer and free
    /// space), the window, the total, and whether the total is an estimate.
    #[must_use]
    pub fn to_wire(&self, measured: Option<u64>) -> Value {
        let total = measured.unwrap_or(self.estimated_total);
        let fixed: u64 = self.fixed.iter().map(|category| category.tokens).sum();
        let total = total.max(fixed);
        let mut categories: Vec<Value> = self
            .fixed
            .iter()
            .filter(|category| category.tokens > 0)
            .map(|category| json!({ "name": category.name, "tokens": category.tokens }))
            .collect();
        let messages = total - fixed;
        if messages > 0 {
            categories.push(json!({ "name": MESSAGES, "tokens": messages }));
        }
        let buffer = self
            .compact_at
            .filter(|_| self.context_window > 0)
            .map_or(0, |at| self.context_window.saturating_sub(at));
        if buffer > 0 {
            categories.push(json!({ "name": AUTOCOMPACT_BUFFER, "tokens": buffer }));
        }
        let free = self
            .context_window
            .saturating_sub(total)
            .saturating_sub(buffer);
        if self.context_window > 0 {
            categories.push(json!({ "name": FREE_SPACE, "tokens": free }));
        }
        json!({
            "categories": categories,
            "contextWindow": self.context_window,
            "totalTokens": total,
            "estimated": measured.is_none(),
            "model": self.model,
        })
    }
}

impl super::engine::SessionEngine {
    /// `/context`'s category breakdown for the live session (the daemon's
    /// `get_context_tree` folds in the measured context).
    pub async fn context_breakdown(&self) -> ContextBreakdown {
        let state = self.session.agent().state().await;
        let context = match self.session.context_rows().cloned() {
            Some(rows) => tokio::task::spawn_blocking(move || rows.current_context())
                .await
                .unwrap_or_default(),
            None => SessionContext::default(),
        };
        let auto_memory = self
            .instructions
            .auto_memory
            .as_ref()
            .map(|memory| memory.index.as_str());
        let fixed = fixed_categories(&BreakdownInputs {
            system_prompt: &state.system_prompt,
            auto_memory,
            tools: state
                .tools
                .iter()
                .map(|tool| (tool.name(), tool.description(), tool.parameters()))
                .collect(),
            context: &context,
        });
        let estimate = super::compaction::estimate_context_tokens(&state.messages);
        // Without a provider usage the message estimate leaves out the
        // request's fixed parts.
        let estimated_total = match estimate.last_usage_index {
            Some(_) => estimate.tokens,
            None => {
                estimate.tokens
                    + fixed
                        .iter()
                        .filter(|category| {
                            matches!(category.name, SYSTEM_PROMPT | SYSTEM_TOOLS)
                        })
                        .map(|category| category.tokens)
                        .sum::<u64>()
            }
        };
        let thinking = super::provider_adapter::model_thinking_level(state.thinking_level);
        let status = self.session.context_limit_status(
            &state.model,
            super::compaction::request_output_budget(&state.model, thinking),
        );
        ContextBreakdown {
            fixed,
            context_window: state.model.context_window,
            compact_at: status.compact_at.filter(|_| status.enabled),
            estimated_total,
            model: state.model.id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompts::session_context::{InstructionFile, SkillEntry};

    fn context() -> SessionContext {
        SessionContext {
            instruction_files: vec![InstructionFile {
                path: "/repo/AGENTS.md".into(),
                content: "x".repeat(400),
            }],
            skills: vec![SkillEntry {
                name: "tasks".into(),
                xml: "<skill>tasks</skill>".into(),
            }],
            mcp_servers: vec!["notion".into()],
            date: "2026-10-18".into(),
            ..SessionContext::default()
        }
    }

    #[test]
    fn categories_estimate_each_input_and_move_memory_out_of_the_prompt() {
        let parameters = json!({ "type": "object" });
        let context = context();
        let categories = fixed_categories(&BreakdownInputs {
            system_prompt: &"p".repeat(4000),
            auto_memory: Some(&"m".repeat(400)),
            tools: vec![("python_repl", "Run Python.", &parameters)],
            context: &context,
        });
        let tokens = |name: &str| {
            categories
                .iter()
                .find(|category| category.name == name)
                .unwrap()
                .tokens
        };
        let names: Vec<&str> = categories.iter().map(|category| category.name).collect();
        assert_eq!(
            names,
            [
                SYSTEM_PROMPT,
                SYSTEM_TOOLS,
                MCP_SERVERS,
                CUSTOM_AGENTS,
                MEMORY_FILES,
                SKILLS
            ]
        );
        // 1000 prompt tokens less 100 of memory, plus the environment rows.
        assert!(tokens(SYSTEM_PROMPT) > 900 && tokens(SYSTEM_PROMPT) < 1000);
        assert_eq!(tokens(SYSTEM_TOOLS), 3 + 3 + 5);
        assert!(tokens(MEMORY_FILES) > 200);
        assert!(tokens(SKILLS) > 0 && tokens(MCP_SERVERS) > 0);
        assert_eq!(tokens(CUSTOM_AGENTS), 0);
    }

    #[test]
    fn the_wire_form_fills_messages_buffer_and_free_space() {
        let breakdown = ContextBreakdown {
            fixed: vec![
                ContextCategory {
                    name: SYSTEM_PROMPT,
                    tokens: 3_000,
                },
                ContextCategory {
                    name: CUSTOM_AGENTS,
                    tokens: 0,
                },
                ContextCategory {
                    name: SKILLS,
                    tokens: 1_000,
                },
            ],
            context_window: 100_000,
            compact_at: Some(80_000),
            estimated_total: 9_000,
            model: "m".into(),
        };
        assert_eq!(
            breakdown.to_wire(Some(24_000)),
            json!({
                "categories": [
                    { "name": SYSTEM_PROMPT, "tokens": 3_000 },
                    { "name": SKILLS, "tokens": 1_000 },
                    { "name": MESSAGES, "tokens": 20_000 },
                    { "name": AUTOCOMPACT_BUFFER, "tokens": 20_000 },
                    { "name": FREE_SPACE, "tokens": 56_000 },
                ],
                "contextWindow": 100_000,
                "totalTokens": 24_000,
                "estimated": false,
                "model": "m",
            })
        );
        // No measurement: the session's estimate; a total below the fixed
        // parts never goes negative.
        let estimated = breakdown.to_wire(None);
        assert_eq!(estimated["totalTokens"], 9_000);
        assert_eq!(estimated["estimated"], true);
        assert_eq!(breakdown.to_wire(Some(10))["totalTokens"], 4_000);
    }
}
