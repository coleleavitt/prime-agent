//! System-prompt assembly: a **cached static prefix** of the human-editable
//! layer files ([`layers`]) that never varies per session (providers can
//! cache it), then a short **session tail** of facts fixed for the session's
//! life (packages, the session role, the appended prompt).
//!
//! Volatile context (instruction files, skills, MCP servers, environment,
//! date) is [`session_context`]: hidden conversation rows on the first turn
//! and deltas when it changes, so the whole system prompt stays byte-stable.

pub mod layers;

pub mod model_prompts;

pub mod output_style;
pub mod session_context;

pub mod system_prompt;

pub use system_prompt::{
    BuildSystemPromptOptions,
    PromptSegment,
    REFINE_SKILL_NAME,
    SegmentKind,
    SystemPromptBreakdown,
    build_system_prompt,
    system_prompt_breakdown,
};
