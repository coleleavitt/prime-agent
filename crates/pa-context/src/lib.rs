//! The context manager: `CortexKit` magic-context's context features as a
//! prime-agent fork feature (design: `docs/context-manager-design.md`).
//!
//! Every model request of a top-level session passes through
//! [`ContextFeature`]'s request-context hook: user messages and tool
//! outputs carry stable `§N§` tags, outputs the model dropped with
//! `ctx.reduce` render as one placeholder, a background historian folds old
//! history into compartments rendered at a tier that decays with age
//! (`decay`, vendored from magic-context), and `ctx.expand` brings any of it
//! back. Queued changes land only on requests whose prompt cache is cold
//! anyway, so routine turns replay the previous request's bytes.

#![forbid(unsafe_code)]

#[expect(
    clippy::cast_lossless,
    reason = "vendored unchanged from magic-context; its casts are on small decay inputs"
)]
#[cfg_attr(
    test,
    expect(
        clippy::cast_possible_truncation,
        reason = "the vendored golden test indexes a small fixture"
    )
)]
mod decay;
mod feature;
mod historian;
mod host;
mod message;
mod nudge;
mod render;
mod session;
mod state;
mod transform;

pub use feature::{CONTEXT_MANAGER_ENV, CTX_SKILL, ContextFeature};
