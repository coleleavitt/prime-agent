//! The kernel half of plan mode: which confinement enforces it in the
//! session's kernel on this machine ([`PlanEnforcement`]).
//!
//! Where the OS can confine processes (Landlock, Seatbelt), plan mode is a
//! sandbox policy: the kernel restarts under `read-only` (see
//! [`crate::os_sandbox::SessionSandbox::for_plan_mode`]), keeping its namespace
//! through the state snapshot, and the `bash()` jobs and MCP stdio servers it
//! starts afterwards inherit that policy. Where it cannot, the fallback is the
//! runtime's in-kernel write guard (`rlm.plan_guard`, see
//! `prime-agent-runtime/src/rlm/repl.md` -> Plan guard), armed through the
//! host-only `plan_guard` frame, with every `bash()` job refused.
//!
//! The session owns one [`PlanModeSwitch`]; every kernel it boots reads the
//! switch when it starts, and a toggle re-applies it to a running kernel. The
//! fallback frame's token is minted per kernel process by the host and never
//! enters a cell, so kernel code cannot switch the guard off.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::os_sandbox::SessionSandbox;

/// How long the host waits for the runtime to answer a `plan_guard` frame.
/// The runtime answers it on its reader thread (never behind a cell); a
/// runtime that predates the frame answers nothing the host can route.
pub(crate) const PLAN_GUARD_SETTLE_TIMEOUT_MS: u64 = 15_000;

/// One session's plan-mode flag, shared by the session engine (tool refusal,
/// per-turn notice) and the kernel lifecycle (the runtime guard). Cloning
/// shares the flag.
#[derive(Debug, Clone, Default)]
pub struct PlanModeSwitch {
    enabled: Arc<AtomicBool>,
}

impl PlanModeSwitch {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
        }
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn set(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// Set the flag and return the previous value.
    #[must_use]
    pub fn replace(&self, enabled: bool) -> bool {
        self.enabled.swap(enabled, Ordering::SeqCst)
    }
}

/// How plan mode is enforced in a session's kernel on this machine.
#[derive(Debug, Clone)]
pub enum PlanEnforcement {
    /// The kernel runs under this OS sandbox while plan mode is on (and the
    /// `bash()` jobs and MCP stdio servers started meanwhile do too). Its
    /// machine support is the configured sandbox's: an unavailable one keeps
    /// the kernel from starting at all.
    Sandbox(SessionSandbox),
    /// No OS sandbox on this machine (and none configured): the runtime's
    /// in-kernel write guard, with every `bash()` job refused. Python-level
    /// writes and spawns raise `PlanModeError`; `ctypes` can get past it.
    KernelGuard {
        /// Why the OS sandbox is unavailable.
        reason: String,
    },
}

impl PlanEnforcement {
    /// The enforcement for a session whose configured sandbox is `configured`.
    #[must_use]
    pub fn resolve(configured: Option<&SessionSandbox>) -> Self {
        Self::from_plan_sandbox(configured, SessionSandbox::for_plan_mode(configured))
    }

    /// The enforcement given plan mode's sandbox, as assessed: only a session
    /// with no configured sandbox falls back to the kernel guard (a configured
    /// one this machine cannot enforce refuses to start the kernel instead).
    pub(crate) fn from_plan_sandbox(
        configured: Option<&SessionSandbox>,
        plan: SessionSandbox,
    ) -> Self {
        match (configured, plan.unavailable_reason()) {
            (None, Some(reason)) => {
                tracing::warn!(
                    target: "pa_core::kernel",
                    %reason,
                    "plan mode falls back to the in-kernel write guard: no OS sandbox"
                );
                PlanEnforcement::KernelGuard { reason }
            }
            (Some(_), _) | (None, None) => PlanEnforcement::Sandbox(plan),
        }
    }
}

/// A session's plan mode: the switch, and how this machine enforces it.
/// Cloning shares the switch.
#[derive(Debug, Clone)]
pub struct PlanMode {
    pub switch: PlanModeSwitch,
    pub enforcement: PlanEnforcement,
}

impl PlanMode {
    /// `switch` enforced as [`PlanEnforcement::resolve`] decides for
    /// `configured`.
    #[must_use]
    pub fn resolve(switch: PlanModeSwitch, configured: Option<&SessionSandbox>) -> Self {
        Self {
            switch,
            enforcement: PlanEnforcement::resolve(configured),
        }
    }

    /// The sandbox a process started now runs under: plan mode's while it is
    /// on and OS-enforced, else `configured`.
    #[must_use]
    pub fn spawn_sandbox(&self, configured: Option<&SessionSandbox>) -> Option<SessionSandbox> {
        match &self.enforcement {
            PlanEnforcement::Sandbox(plan) if self.switch.is_enabled() => Some(plan.clone()),
            PlanEnforcement::Sandbox(_) | PlanEnforcement::KernelGuard { .. } => {
                configured.cloned()
            }
        }
    }

    /// Why plan mode is not OS-enforced here, when it falls back to the
    /// in-kernel guard.
    #[must_use]
    pub fn fallback_reason(&self) -> Option<&str> {
        match &self.enforcement {
            PlanEnforcement::KernelGuard { reason } => Some(reason),
            PlanEnforcement::Sandbox(_) => None,
        }
    }
}

/// The in-kernel fallback guard a kernel manager applies
/// ([`PlanEnforcement::KernelGuard`]): the session's switch, the host-owned
/// directories the guarded kernel must still write (the session artifact dir
/// holding the namespace snapshot), the directories that stay read-only even
/// inside a default writable root (the workspace: a checkout under a temp dir
/// is still the user's code), and why there is no OS sandbox (the refusal
/// every `bash()` job meets while the guard is armed).
#[derive(Debug, Clone, Default)]
pub struct KernelPlanGuard {
    pub mode: PlanModeSwitch,
    pub writable_roots: Vec<PathBuf>,
    pub protected_roots: Vec<PathBuf>,
    pub no_sandbox_reason: String,
}

impl KernelPlanGuard {
    /// What a `bash()` job meets while the guard is armed.
    pub(crate) fn job_refusal(&self) -> String {
        format!(
            "Plan mode is active: running commands is blocked (this machine has no OS sandbox to \
             run them read-only: {}). Present your plan or answer, and ask the user to exit plan \
             mode if edits are needed.",
            self.no_sandbox_reason
        )
    }
}

/// A fresh per-kernel token: 32 random bytes, hex-encoded.
///
/// # Errors
///
/// Returns an error when the OS random source fails.
pub(crate) fn mint_plan_guard_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("plan guard token: OS randomness failed: {error}"))?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_clones_share_one_flag() {
        let switch = PlanModeSwitch::new(false);
        let shared = switch.clone();
        assert!(!shared.replace(true));
        assert!(switch.is_enabled());
    }

    fn assessed(supported: bool) -> SessionSandbox {
        SessionSandbox::for_plan_mode_with(None, |_| {
            if supported {
                Ok(pa_os_sandbox::Assessment {
                    mechanism: "Landlock ABI 6".to_string(),
                    gaps: Vec::new(),
                })
            } else {
                Err(pa_os_sandbox::SandboxError::Unsupported {
                    reason: "no Landlock".to_string(),
                })
            }
        })
    }

    /// Only a session without a configured sandbox falls back to the kernel
    /// guard; one with a configured sandbox keeps the OS policy (its kernel
    /// refuses to start where that is unavailable).
    #[test]
    fn an_unsupported_sandbox_falls_back_to_the_kernel_guard_only_when_none_is_configured() {
        let configured = assessed(true);
        let describe = |enforcement: PlanEnforcement| match enforcement {
            PlanEnforcement::Sandbox(sandbox) => format!("sandbox {}", sandbox.status_label()),
            PlanEnforcement::KernelGuard { reason } => format!("kernel guard: {reason}"),
        };
        assert_eq!(
            [
                describe(PlanEnforcement::from_plan_sandbox(None, assessed(true))),
                describe(PlanEnforcement::from_plan_sandbox(None, assessed(false))),
                describe(PlanEnforcement::from_plan_sandbox(
                    Some(&configured),
                    assessed(false)
                )),
            ],
            [
                "sandbox read-only+net".to_string(),
                "kernel guard: OS sandbox unavailable: no Landlock".to_string(),
                "sandbox read-only+net (unavailable)".to_string(),
            ]
        );
    }

    #[test]
    fn spawns_use_the_plan_sandbox_only_while_plan_mode_is_on() {
        let plan = PlanMode {
            switch: PlanModeSwitch::new(false),
            enforcement: PlanEnforcement::Sandbox(assessed(true)),
        };
        let fallback = PlanMode {
            switch: plan.switch.clone(),
            enforcement: PlanEnforcement::KernelGuard {
                reason: "no Landlock".to_string(),
            },
        };
        let modes = |plan: &PlanMode| plan.spawn_sandbox(None).map(|sandbox| sandbox.mode());
        let off = (modes(&plan), modes(&fallback));
        plan.switch.set(true);
        assert_eq!(
            (off, (modes(&plan), modes(&fallback))),
            (
                (None, None),
                (Some(crate::os_sandbox::SandboxMode::ReadOnly), None)
            )
        );
    }

    #[test]
    fn tokens_are_distinct_hex() {
        let first = mint_plan_guard_token().unwrap();
        let second = mint_plan_guard_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
