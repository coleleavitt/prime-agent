//! The guard pipeline: every guard reads the text the shell will run, in a
//! fixed order, and the first refusal wins.

use std::collections::BTreeSet;

use crate::context::GuardContext;
use crate::guards;
use crate::script::Script;
use crate::verdict::{GuardKind, Refusal};

/// The guards one call bypasses (`bash(command, allow_sudo=True, ...)`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allowances(BTreeSet<GuardKind>);

impl Allowances {
    /// No guard bypassed.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Bypass `guard` for this call.
    #[must_use]
    pub fn allow(mut self, guard: GuardKind) -> Self {
        self.0.insert(guard);
        self
    }

    #[must_use]
    pub fn allows(&self, guard: GuardKind) -> bool {
        self.0.contains(&guard)
    }
}

/// Run every guard that is neither allowed for this call nor bypassed at
/// kernel start.
///
/// # Errors
///
/// The first guard's [`Refusal`], with its late-bypass warning when the
/// guard's bypass variable appeared after kernel start.
pub fn check(
    script: &Script<'_>,
    allow: &Allowances,
    context: &GuardContext,
) -> Result<(), Refusal> {
    for guard in GuardKind::ALL {
        if allow.allows(guard) || context.launch_bypassed(guard) {
            continue;
        }
        guards::check(guard, script, context)?;
    }
    Ok(())
}
