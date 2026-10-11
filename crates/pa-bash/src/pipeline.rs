//! The guard pipeline: the text the shell will run is parsed once into the
//! command model, every guard judges that model in a fixed order, and the
//! first refusal wins.

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::context::GuardContext;
use crate::guards::{self, Check, Verdict};
use crate::model::Model;
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
    let active: Vec<GuardKind> = GuardKind::ALL
        .into_iter()
        .filter(|guard| !allow.allows(*guard) && !context.launch_bypassed(*guard))
        .collect();
    if active.is_empty() {
        return Ok(());
    }
    // A bug in the model or a rule must not leave the request unanswered:
    // a panic refuses (fail closed) and names the guard that hit it.
    let Ok(model) = catch_unwind(AssertUnwindSafe(|| Model::build(script, context))) else {
        return Err(internal_error(active[0]));
    };
    let check = Check {
        model: &model,
        script,
        context,
    };
    for guard in active {
        match catch_unwind(AssertUnwindSafe(|| guards::judge(guard, &check))) {
            Ok(Verdict::Refuse(refusal)) => return Err(refusal),
            Ok(Verdict::Allow) => {}
            Err(_) => return Err(internal_error(guard)),
        }
    }
    Ok(())
}

fn internal_error(guard: GuardKind) -> Refusal {
    Refusal::new(
        guard,
        format!(
            "Refusing to run this command: the {} check failed while reading it (an internal error in the guard). Retry with bash(command, {}=True) if the command is safe to run.",
            guard.key(),
            guard.allow_kwarg()
        ),
    )
}
