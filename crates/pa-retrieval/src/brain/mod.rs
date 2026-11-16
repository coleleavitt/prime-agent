//! The brain: one searchable, curated view over every memory store this
//! agent dir and `CortexKit` magic-context hold (global and session-local
//! harness memories, auto-memory and personal files, magic-context rows),
//! with pins, excludes, supersession and synthesized per-scope summaries.
//! Design: `docs/brain-design.md`.

mod controls;
mod corpus;
mod demote;
mod hook;
mod host;
mod mc;
mod project;
mod search;
mod supersede;
mod synth;

pub(crate) use controls::CachedState;
pub(crate) use demote::demote_superseded;
pub(crate) use hook::BrainPromptHook;
pub(crate) use host::{BrainSession, register, synthesize};
pub(crate) use project::project_of;
pub(crate) use synth::{SynthScope, auto_disabled, due};

/// The bundled skill (`skills/.features/brain`).
pub(crate) const BRAIN_SKILL: &str = "brain";

/// The automatic synthesis's gate state for `scope`.
pub(crate) fn summary_of(
    agent_dir: &std::path::Path,
    scope: &SynthScope,
) -> Option<controls::ScopeSummary> {
    controls::read(agent_dir)
        .synthesis
        .scopes
        .get(&scope.key())
        .cloned()
}
