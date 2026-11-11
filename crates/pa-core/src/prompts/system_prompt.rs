//! Layered system-prompt assembly: static layer files first (cacheable),
//! then the few session facts that stay fixed for the session's life. See
//! the parent module docs.
//!
//! Volatile context (instruction files, the skills inventory, MCP servers,
//! the environment, the date) is not here: it reaches the model as
//! conversation rows ([`super::session_context`]), so the system prompt is
//! byte-stable for the whole session and across sessions that share these
//! inputs, whatever their working directory, date or project.

use super::layers;

/// The bundled skill the refinement trigger guidance keys on.
pub const REFINE_SKILL_NAME: &str = "refine";

/// Whether a prompt segment belongs to the cache-stable prefix or the
/// session-specific tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// Cacheable static prefix content (layer files, or a user replacement).
    Static,
    /// Session-specific tail content; must never leak into [`SegmentKind::Static`].
    Dynamic,
}

/// One named slice of the assembled prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSegment {
    /// Stable segment name (`core`, `usage`, `packages`, `session-role`, ...).
    pub name: &'static str,
    pub kind: SegmentKind,
    /// Provenance: the layer file, or what generated the segment.
    pub source: &'static str,
    pub text: String,
}

impl PromptSegment {
    fn static_segment(name: &'static str, source: &'static str, text: String) -> Self {
        Self {
            name,
            kind: SegmentKind::Static,
            source,
            text,
        }
    }

    fn dynamic_segment(name: &'static str, source: &'static str, text: String) -> Self {
        Self {
            name,
            kind: SegmentKind::Dynamic,
            source,
            text,
        }
    }
}

/// The assembled prompt plus its per-layer breakdown. `cached_prefix_len` is
/// the byte length of the static prefix: `assembled[..cached_prefix_len]` is
/// byte-identical across every session that shares the same custom-prompt
/// and model-selection inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPromptBreakdown {
    pub segments: Vec<PromptSegment>,
    pub assembled: String,
    pub cached_prefix_len: usize,
}

/// Inputs for one session's prompt. Static inputs (`custom_prompt`,
/// `model_prompt_extras`) select the cached prefix; the rest are session
/// facts fixed for the session's life.
#[derive(Debug, Default)]
pub struct BuildSystemPromptOptions<'a> {
    /// Replaces the layered static prefix with user text. The tail still
    /// applies.
    pub custom_prompt: Option<String>,
    pub model_prompt_extras: Option<&'a str>,
    /// Active tools. Tool schemas carry tool descriptions outside the prompt.
    pub selected_tools: Option<Vec<&'a str>>,
    pub append_system_prompt: Option<String>,
    /// Whether to include the subagent surface in this session.
    pub allow_recursion: Option<bool>,
    /// Fixed recursive-agent depth for this session.
    pub rlm_depth: Option<u32>,
    /// Human-readable parent name or id for child communication doctrine.
    pub rlm_parent_agent: Option<&'a str>,
    /// Whether the session runs without a daemon (no scheduler, no
    /// heartbeat firing). The wake-up facts the daemonless line states
    /// must match the embedding's wiring.
    pub daemonless: bool,
    /// Whether `rlm.spawn` can create children in this session (the
    /// daemonless print host or a daemon worker); a daemonless session
    /// without one cannot spawn at all.
    pub can_spawn: bool,
    /// The selected output style (`None`: the default style), fixed for
    /// the session (a mid-session switch rides a hidden row). Its block is
    /// the first tail segment; a style that does not keep the coding
    /// instructions drops the opinionated layer.
    pub output_style: Option<super::output_style::OutputStyle>,
    /// The auto-memory section (its `MEMORY.md` index included), when the
    /// session carries auto memory. Read once at session start, so it is
    /// fixed for the session (Claude Code keeps memory in the system
    /// prompt too); it varies by project, never by turn.
    pub auto_memory: Option<String>,
    /// Coordinator mode's section (Claude Code's coordinator prompt), for a
    /// root session in coordinator mode.
    pub coordinator_section: Option<String>,
    /// Whether the session starts in brief mode (Claude Code's "Talking to
    /// the user" section; a mid-session `/brief` rides a hidden row).
    pub brief_mode: bool,
}

/// Build the system prompt (assembled text only).
#[must_use]
pub fn build_system_prompt(options: &BuildSystemPromptOptions) -> String {
    system_prompt_breakdown(options).assembled
}

/// Build the system prompt with its per-layer breakdown.
#[must_use]
pub fn system_prompt_breakdown(options: &BuildSystemPromptOptions) -> SystemPromptBreakdown {
    let mut segments: Vec<PromptSegment> = Vec::new();

    // The static prefix: the user's replacement prompt, or the layered files.
    match options.custom_prompt.as_deref() {
        Some(custom) if !custom.is_empty() => {
            segments.push(PromptSegment::static_segment(
                "custom",
                "--system-prompt",
                custom.to_string(),
            ));
        }
        _ => {
            segments.push(PromptSegment::static_segment(
                "core",
                layers::layer_source("core").unwrap_or_default(),
                layers::CORE_LAYER.trim().to_string(),
            ));
            segments.push(PromptSegment::static_segment(
                "usage",
                layers::layer_source("usage").unwrap_or_default(),
                layers::USAGE_LAYER.trim().to_string(),
            ));
            // Claude Code drops its coding instructions under a style
            // without `keep-coding-instructions`; the opinionated layer is
            // this prompt's equivalent.
            let keeps_coding_instructions = options
                .output_style
                .as_ref()
                .is_none_or(|style| style.keep_coding_instructions);
            if keeps_coding_instructions {
                segments.push(PromptSegment::static_segment(
                    "opinionated",
                    layers::layer_source("opinionated").unwrap_or_default(),
                    layers::OPINIONATED_LAYER.trim().to_string(),
                ));
            }
            if let Some(extras) = options.model_prompt_extras {
                segments.push(PromptSegment::static_segment(
                    "model-prompts",
                    layers::layer_source("model-prompts").unwrap_or_default(),
                    extras.to_string(),
                ));
            }
        }
    }
    let cached_prefix_len = segments
        .iter()
        .map(|segment| segment.text.len())
        .sum::<usize>()
        + 2 * segments.len().saturating_sub(1);

    // The session tail, in fixed order: output style -> packages -> auto
    // memory -> session role -> coordinator -> brief -> appended prompt.
    // Each is fixed for the
    // session's life.
    let tools: Vec<&str> = options
        .selected_tools
        .clone()
        .unwrap_or_else(|| vec![pa_types::ai::PYTHON_TOOL_NAME]);
    let has_ipython = tools.contains(&pa_types::ai::PYTHON_TOOL_NAME);

    if let Some(style) = &options.output_style {
        segments.push(PromptSegment::dynamic_segment(
            "output-style",
            "outputStyle setting",
            super::output_style::output_style_section(style),
        ));
    }

    segments.push(PromptSegment::dynamic_segment(
        "packages",
        "kernel bootstrap defaults",
        packages_section(),
    ));

    if let Some(memory) = options
        .auto_memory
        .as_deref()
        .filter(|text| !text.is_empty())
    {
        segments.push(PromptSegment::dynamic_segment(
            "auto-memory",
            "auto-memory directory",
            memory.to_string(),
        ));
    }

    let role = session_role_section(options, has_ipython);
    if !role.is_empty() {
        segments.push(PromptSegment::dynamic_segment(
            "session-role",
            "RLM recursion state",
            role,
        ));
    }

    if let Some(section) = options
        .coordinator_section
        .as_deref()
        .filter(|text| !text.is_empty())
    {
        segments.push(PromptSegment::dynamic_segment(
            "coordinator",
            "experimental.coordinatorMode",
            section.to_string(),
        ));
    }

    if options.brief_mode {
        segments.push(PromptSegment::dynamic_segment(
            "brief",
            "experimental.briefMode",
            crate::session_engine::brief_mode::BRIEF_SECTION.to_string(),
        ));
    }

    if let Some(extra) = options.append_system_prompt.as_deref() {
        if !extra.is_empty() {
            segments.push(PromptSegment::dynamic_segment(
                "appended-prompt",
                "--append-system-prompt",
                extra.to_string(),
            ));
        }
    }

    let assembled = segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    SystemPromptBreakdown {
        segments,
        assembled,
        cached_prefix_len,
    }
}

fn packages_section() -> String {
    use crate::kernel::bootstrap::default_rlm_extra_import_labels;
    let mut lines = vec![format!(
        "Pre-installed Python packages: {}.",
        default_rlm_extra_import_labels().join(", ")
    )];
    lines.push(
        "Install additional packages with `uv pip install <pkg>` (this is a uv-managed venv with no pip module)."
            .to_string(),
    );
    lines.join("\n")
}

fn session_role_section(options: &BuildSystemPromptOptions, has_ipython: bool) -> String {
    let depth = options.rlm_depth.unwrap_or(0);
    let mut lines = vec![format!(
        "Recursive agent depth: {depth}{}",
        if depth == 0 { " (root)" } else { " (not root)" }
    )];
    if options.daemonless {
        lines.push(if options.can_spawn {
            "This session has no daemon: heartbeats never fire, and background `bash()` completions wake you with a queued `[bash-done]` turn; the run waits for live background commands and your subagents before it exits. Subagent replies and exit notices still wake you."
                .to_string()
        } else {
            "This session has no daemon: `rlm.spawn` is unavailable, heartbeats never fire, and background `bash()` completions wake you with a queued `[bash-done]` turn."
                .to_string()
        });
    }
    if !has_ipython {
        lines.push(
            "This session has no Python REPL (`python_repl` tool): the programmatic tools described above are unavailable here."
                .to_string(),
        );
    }
    if options.allow_recursion == Some(false) {
        lines.push("Subagent spawning is disabled in this session.".to_string());
    }
    if depth > 0 {
        lines.push(format!(
            "You are a child agent spawned by {}. Task prompts are labeled `[task from parent]`.",
            options.rlm_parent_agent.unwrap_or("your parent agent")
        ));
        if has_ipython {
            lines.push(
                "When a task calls for an answer, reply explicitly with `await agent_message.send(message, receiver_role=\"parent\")`. Not every message or task needs a reply; continue cleanup after sending and go idle normally.".to_string(),
            );
            lines.push(
                "For long-running work, report brief progress with `await rlm.progress_note('...')` (at most 512 characters, throttled to about one note per 10 seconds); the parent sees notes without needing a reply.".to_string(),
            );
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_options() -> BuildSystemPromptOptions<'static> {
        BuildSystemPromptOptions::default()
    }

    #[test]
    fn default_prompt_is_layered_with_static_prefix_first() {
        let breakdown = system_prompt_breakdown(&base_options());
        let prompt = &breakdown.assembled;
        assert!(prompt.starts_with("# prime-agent harness"));
        assert_eq!(breakdown.segments[0].kind, SegmentKind::Static);
        assert_eq!(breakdown.segments[0].name, "core");
        let names: Vec<&str> = breakdown
            .segments
            .iter()
            .map(|segment| segment.name)
            .collect();
        assert_eq!(
            names,
            ["core", "usage", "opinionated", "packages", "session-role"],
            "core/usage/opinionated layers, then the session tail"
        );
        assert_eq!(
            &prompt[..breakdown.cached_prefix_len],
            format!(
                "{}\n\n{}\n\n{}",
                layers::CORE_LAYER.trim(),
                layers::USAGE_LAYER.trim(),
                layers::OPINIONATED_LAYER.trim()
            )
        );
        let tail = &prompt[breakdown.cached_prefix_len..];
        assert!(tail.contains("Recursive agent depth: 0 (root)"));
        assert!(tail.contains("Pre-installed Python packages: requests, httpx,"));
    }

    #[test]
    fn the_prompt_carries_no_volatile_context() {
        // Environment, date, instruction files, skills and MCP servers are
        // conversation rows now: none of their markers may reach the prompt.
        let prompt = build_system_prompt(&base_options());
        for marker in [
            "Working directory:",
            "Current date:",
            "Today's date is",
            "Conversation log:",
            "# Project Context",
            "<available_skills>",
            "# Generic MCP Connections",
            "# Environment",
        ] {
            assert!(
                !prompt.contains(marker),
                "{marker:?} leaked into the prompt"
            );
        }
    }

    #[test]
    fn output_style_leads_the_tail_and_may_drop_the_coding_guidelines() {
        let style = |keep: bool| super::super::output_style::OutputStyle {
            name: "pirate".to_string(),
            description: "Arr".to_string(),
            prompt: "Answer as a pirate.".to_string(),
            source: super::super::output_style::OutputStyleSource::User,
            keep_coding_instructions: keep,
            turn_reminder: None,
        };
        let names = |options: &BuildSystemPromptOptions| -> Vec<&'static str> {
            system_prompt_breakdown(options)
                .segments
                .iter()
                .map(|segment| segment.name)
                .take(4)
                .collect()
        };
        let mut options = base_options();
        options.output_style = Some(style(false));
        assert_eq!(
            names(&options),
            ["core", "usage", "output-style", "packages"]
        );
        assert!(system_prompt_breakdown(&options).assembled.contains(
            "# Output Style: pirate
Answer as a pirate."
        ));
        options.output_style = Some(style(true));
        assert_eq!(
            names(&options),
            ["core", "usage", "opinionated", "output-style"]
        );
    }

    #[test]
    fn auto_memory_follows_the_packages_in_the_tail() {
        let mut options = base_options();
        options.auto_memory = Some(
            "# auto memory
index"
                .to_string(),
        );
        let breakdown = system_prompt_breakdown(&options);
        let names: Vec<&str> = breakdown
            .segments
            .iter()
            .map(|segment| segment.name)
            .collect();
        let packages = names.iter().position(|name| *name == "packages").unwrap();
        assert_eq!(names[packages + 1], "auto-memory");
        assert_eq!(breakdown.segments[packages + 1].kind, SegmentKind::Dynamic);
        assert!(breakdown.assembled[breakdown.cached_prefix_len..].contains(
            "# auto memory
index"
        ));
    }

    #[test]
    fn the_child_role_is_in_the_tail() {
        let mut other = base_options();
        other.rlm_depth = Some(2);
        other.rlm_parent_agent = Some("the lead");
        let left = system_prompt_breakdown(&base_options());
        let right = system_prompt_breakdown(&other);
        assert_eq!(
            left.assembled[..left.cached_prefix_len],
            right.assembled[..right.cached_prefix_len]
        );
        assert!(
            right.assembled[right.cached_prefix_len..]
                .contains("You are a child agent spawned by the lead.")
        );
    }

    #[test]
    fn custom_prompt_replaces_layers_keeps_tail() {
        let mut options = base_options();
        options.custom_prompt = Some("Be terse.".to_string());
        options.model_prompt_extras = Some("Tier-2 guidance.");
        let breakdown = system_prompt_breakdown(&options);
        let prompt = &breakdown.assembled;
        assert!(prompt.starts_with("Be terse."));
        assert_eq!(breakdown.segments[0].name, "custom");
        assert!(prompt.contains("Recursive agent depth: 0 (root)"));
        assert!(!prompt.contains("# prime-agent harness"));
        assert!(!prompt.contains("Tier-2 guidance."));
    }

    #[test]
    fn the_appended_prompt_ends_the_prompt() {
        let mut options = base_options();
        options.append_system_prompt = Some("Always say ZULU.".to_string());
        let prompt = build_system_prompt(&options);
        assert!(prompt.ends_with("\n\nAlways say ZULU."));
    }

    #[test]
    fn daemonless_line_drops_the_stale_no_wake_claim() {
        // The line's one durable contract: it no longer tells daemonless
        // agents that background completions never wake them.
        let mut spawning = base_options();
        spawning.daemonless = true;
        spawning.can_spawn = true;
        assert!(!build_system_prompt(&spawning).contains("completions do not wake you"));

        let mut bare = base_options();
        bare.daemonless = true;
        assert!(!build_system_prompt(&bare).contains("completions do not wake you"));
    }

    #[test]
    fn model_prompt_extras_form_the_last_static_segment() {
        let mut options = base_options();
        options.model_prompt_extras = Some("Tier-2 guidance.");
        let breakdown = system_prompt_breakdown(&options);
        let names: Vec<&str> = breakdown
            .segments
            .iter()
            .map(|segment| segment.name)
            .collect();
        assert_eq!(
            names[..5],
            ["core", "usage", "opinionated", "model-prompts", "packages"],
            "extras are the last static segment, directly before the session tail"
        );
    }
}
