//! OS-level confinement for the processes Prime Agent runs on the model's
//! behalf: the Python kernel (and so everything it spawns) and the `!`
//! user-bash lane.
//!
//! - Linux: Landlock filesystem rules (plus TCP rules where the ABI has
//!   them) and, with network off, a seccomp filter refusing every socket
//!   outside the unix domain. Applied by the exec'd launcher ([`launch`])
//!   when the host registered one, else in the child between fork and exec.
//! - macOS: the child runs under `/usr/bin/sandbox-exec` with a generated
//!   Seatbelt profile.
//! - Windows and everything else: [`SandboxError::Unsupported`]; nothing
//!   pretends to confine.
//!
//! Public API: the policy vocabulary ([`SandboxMode`], [`SandboxPolicy`],
//! [`SandboxPaths`]), [`assess`] (what a policy gets on this machine, no
//! side effects), and [`prepare`] → [`PreparedSandbox::command`] (a
//! `Command` whose children are confined); [`session_command`] for a child
//! that must lead its own session; the launcher ([`set_launcher`],
//! [`launch_main`]) that makes both fork-free.

pub mod launch;
mod policy;
// Pure profile generation, compiled (and unit-tested) on every platform.
#[cfg(target_os = "linux")]
mod linux;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod seatbelt;

use std::ffi::OsStr;
use std::process::Command;

pub use launch::{launch_main, launcher, set_launcher, Launcher, LAUNCHER_FLAG};
pub use policy::{Confinement, NetworkAccess, SandboxMode, SandboxPaths, SandboxPolicy};

/// Why a sandbox cannot be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// This platform or kernel cannot confine processes; running anyway
    /// would claim a confinement that does not exist.
    #[error("OS sandbox unavailable: {reason}")]
    Unsupported { reason: String },
    /// Building the restriction failed (an unreadable root, a rejected rule).
    #[error("OS sandbox setup failed ({context}): {message}")]
    Setup {
        context: &'static str,
        message: String,
    },
}

/// What a policy gets on this machine: the enforcing mechanism and every
/// protection it lacks here (empty: full confinement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    /// e.g. `Landlock ABI 6 + seccomp`, `Seatbelt`.
    pub mechanism: String,
    /// Each protection the policy asks for that this kernel cannot enforce.
    pub gaps: Vec<String>,
}

impl Assessment {
    /// Whether some requested protection is missing on this machine.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        !self.gaps.is_empty()
    }
}

/// What `policy` gets on this machine, without preparing anything.
///
/// # Errors
///
/// [`SandboxError::Unsupported`] when this platform or kernel cannot
/// confine processes at all.
pub fn assess(policy: &SandboxPolicy) -> Result<Assessment, SandboxError> {
    platform::assess(policy)
}

/// A restriction ready to apply to any number of spawns.
pub struct PreparedSandbox {
    assessment: Assessment,
    inner: platform::Prepared,
}

impl std::fmt::Debug for PreparedSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedSandbox")
            .field("assessment", &self.assessment)
            .finish_non_exhaustive()
    }
}

impl PreparedSandbox {
    /// What this restriction enforces here.
    #[must_use]
    pub fn assessment(&self) -> &Assessment {
        &self.assessment
    }

    /// A command running `program` confined: the caller adds the arguments,
    /// environment, working directory and stdio as for `Command::new`.
    /// Through the registered launcher when there is one (no fork), else
    /// confined in the forked child.
    #[must_use]
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let request = self.inner.launch_request(program.as_ref());
        if request.confine.is_some() {
            if let Some(command) = launch::launcher_command(&request) {
                return command;
            }
        }
        self.inner.command(program.as_ref())
    }
}

/// A command running `program` (confined by `sandbox` when given) as the
/// leader of a new session, with no controlling terminal, through the
/// registered launcher: `None` without one. Its stdin must be a socket: the
/// launcher writes [`launch::LAUNCH_ACK`] there once the session exists and
/// the restriction is applied (or [`launch::LAUNCH_NAK`] and the error), and
/// the caller reads it before signalling the child's group.
#[must_use]
pub fn session_command(
    program: impl AsRef<OsStr>,
    sandbox: Option<&PreparedSandbox>,
) -> Option<Command> {
    let mut request = match sandbox {
        Some(prepared) => prepared.inner.launch_request(program.as_ref()),
        None => launch::LaunchRequest {
            program: program.as_ref().to_os_string(),
            ..launch::LaunchRequest::default()
        },
    };
    request.setsid = true;
    request.ack_stdin = true;
    launch::launcher_command(&request)
}

/// Prepare `policy` for launches against `paths`: everything that can fail
/// or allocate happens here, before any spawn.
///
/// # Errors
///
/// [`SandboxError::Unsupported`] when this machine cannot confine, and
/// [`SandboxError::Setup`] when a rule cannot be built.
pub fn prepare(
    policy: &SandboxPolicy,
    paths: &SandboxPaths,
) -> Result<PreparedSandbox, SandboxError> {
    let assessment = platform::assess(policy)?;
    let roots = policy.writable_roots_for(paths);
    let inner = platform::Prepared::new(&roots, policy.network)?;
    Ok(PreparedSandbox { assessment, inner })
}

#[cfg(target_os = "linux")]
mod platform {
    use std::ffi::OsStr;
    use std::path::PathBuf;
    use std::process::Command;

    use crate::{Assessment, NetworkAccess, SandboxError, SandboxPolicy};

    pub(crate) fn assess(policy: &SandboxPolicy) -> Result<Assessment, SandboxError> {
        crate::linux::assess(crate::linux::landlock_abi(), policy.network)
    }

    pub(crate) struct Prepared(crate::linux::LinuxSandbox);

    impl Prepared {
        pub(crate) fn new(roots: &[PathBuf], network: NetworkAccess) -> Result<Self, SandboxError> {
            crate::linux::LinuxSandbox::prepare(roots, network).map(Self)
        }

        pub(crate) fn command(&self, program: &OsStr) -> Command {
            let mut command = Command::new(program);
            self.0.apply(&mut command);
            command
        }

        /// The launcher applies the same roots and network rule itself.
        pub(crate) fn launch_request(&self, program: &OsStr) -> crate::launch::LaunchRequest {
            crate::launch::LaunchRequest {
                confine: Some((self.0.roots().to_vec(), self.0.network())),
                program: program.to_os_string(),
                ..crate::launch::LaunchRequest::default()
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use crate::{Assessment, NetworkAccess, SandboxError, SandboxPolicy};

    pub(crate) fn assess(_policy: &SandboxPolicy) -> Result<Assessment, SandboxError> {
        if !Path::new(crate::seatbelt::SANDBOX_EXEC).exists() {
            return Err(SandboxError::Unsupported {
                reason: format!("{} is missing", crate::seatbelt::SANDBOX_EXEC),
            });
        }
        Ok(Assessment {
            mechanism: "Seatbelt".to_string(),
            gaps: Vec::new(),
        })
    }

    pub(crate) struct Prepared {
        launcher_args: Vec<OsString>,
    }

    impl Prepared {
        pub(crate) fn new(roots: &[PathBuf], network: NetworkAccess) -> Result<Self, SandboxError> {
            Ok(Self {
                launcher_args: crate::seatbelt::launcher_args(roots, network),
            })
        }

        pub(crate) fn command(&self, program: &OsStr) -> Command {
            let mut command = Command::new(crate::seatbelt::SANDBOX_EXEC);
            command.args(&self.launcher_args).arg(program);
            command
        }

        /// `sandbox-exec` is already an exec'd launcher: the request only
        /// runs it (for a new session).
        pub(crate) fn launch_request(&self, program: &OsStr) -> crate::launch::LaunchRequest {
            let mut args = self.launcher_args.clone();
            args.push(program.to_os_string());
            crate::launch::LaunchRequest {
                program: crate::seatbelt::SANDBOX_EXEC.into(),
                args,
                ..crate::launch::LaunchRequest::default()
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use std::ffi::OsStr;
    use std::path::PathBuf;
    use std::process::Command;

    use crate::{Assessment, NetworkAccess, SandboxError, SandboxPolicy};

    fn unsupported() -> SandboxError {
        SandboxError::Unsupported {
            reason: format!(
                "OS sandboxing is not supported on {} (Linux and macOS only); set `sandbox.mode` to `off`",
                std::env::consts::OS
            ),
        }
    }

    pub(crate) fn assess(_policy: &SandboxPolicy) -> Result<Assessment, SandboxError> {
        Err(unsupported())
    }

    /// Uninhabited: no confined command can exist on this platform.
    pub(crate) enum Prepared {}

    impl Prepared {
        pub(crate) fn new(
            _roots: &[PathBuf],
            _network: NetworkAccess,
        ) -> Result<Self, SandboxError> {
            Err(unsupported())
        }

        pub(crate) fn command(&self, _program: &OsStr) -> Command {
            match *self {}
        }

        pub(crate) fn launch_request(&self, _program: &OsStr) -> crate::launch::LaunchRequest {
            match *self {}
        }
    }
}
