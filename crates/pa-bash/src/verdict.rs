//! What a guard decides: allow, or a typed refusal carrying the exact message
//! the kernel raises.

use serde::Serialize;

/// The six kernel `bash()` refusal guards, in the order the pipeline runs
/// them (the first refusal wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum GuardKind {
    /// Destructive git discards on a dirty working tree.
    DestructiveGit,
    /// Recursive chmod/chown that can escape the workspace.
    DestructiveChmod,
    /// Force-pushes to a protected target.
    ForcePush,
    /// Commands that echo secrets into the transcript.
    SecretEcho,
    /// Downloads a shell interpreter would run.
    PipeToShell,
    /// sudo/doas privilege escalation.
    Sudo,
}

impl GuardKind {
    /// Every guard, in pipeline order.
    pub const ALL: [GuardKind; 6] = [
        GuardKind::DestructiveGit,
        GuardKind::DestructiveChmod,
        GuardKind::ForcePush,
        GuardKind::SecretEcho,
        GuardKind::PipeToShell,
        GuardKind::Sudo,
    ];

    /// The guard's wire name (`destructive_git`, ..., `sudo`): the kernel's
    /// `allow`/`launchBypass` lists and the parity corpus use it.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            GuardKind::DestructiveGit => "destructive_git",
            GuardKind::DestructiveChmod => "destructive_chmod",
            GuardKind::ForcePush => "force_push",
            GuardKind::SecretEcho => "secret_echo",
            GuardKind::PipeToShell => "pipe_to_shell",
            GuardKind::Sudo => "sudo",
        }
    }

    /// The guard with this wire name.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|guard| guard.key() == key)
    }

    /// The exception class the kernel raises for this guard's refusals
    /// (`rlm.bash.<ErrorName>`, a `RuntimeError` subclass).
    #[must_use]
    pub fn error_name(self) -> &'static str {
        match self {
            GuardKind::DestructiveGit => "DestructiveGitRefusalError",
            GuardKind::DestructiveChmod => "DestructiveChmodRefusalError",
            GuardKind::ForcePush => "ForcePushRefusalError",
            GuardKind::SecretEcho => "SecretEchoRefusalError",
            GuardKind::PipeToShell => "PipeToShellRefusalError",
            GuardKind::Sudo => "PrivilegeEscalationRefusalError",
        }
    }

    /// The environment variable that disables this guard when it is set at
    /// kernel start (read once at launch; a later write is ignored).
    #[must_use]
    pub fn bypass_env(self) -> &'static str {
        match self {
            GuardKind::DestructiveGit => "PI_BASH_ALLOW_DESTRUCTIVE_GIT",
            GuardKind::DestructiveChmod => "PI_BASH_ALLOW_DESTRUCTIVE_CHMOD",
            GuardKind::ForcePush => "PI_BASH_ALLOW_FORCE_PUSH",
            GuardKind::SecretEcho => "PI_BASH_ALLOW_SECRET_ECHO",
            GuardKind::PipeToShell => "PI_BASH_ALLOW_PIPE_TO_SHELL",
            GuardKind::Sudo => "PI_BASH_ALLOW_SUDO",
        }
    }

    /// The `bash()` keyword argument that bypasses this guard for one call.
    #[must_use]
    pub fn allow_kwarg(self) -> &'static str {
        match self {
            GuardKind::DestructiveGit => "allow_destructive_git",
            GuardKind::DestructiveChmod => "allow_destructive_chmod",
            GuardKind::ForcePush => "allow_force_push",
            GuardKind::SecretEcho => "allow_secret_echo",
            GuardKind::PipeToShell => "allow_pipe_to_shell",
            GuardKind::Sudo => "allow_sudo",
        }
    }
}

/// A refused command: which guard refused it, the message the kernel raises,
/// and the one-time stderr warning the kernel prints when the guard's bypass
/// variable appeared after kernel start (the client prints it at most once
/// per guard).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Refusal {
    pub guard: GuardKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub late_bypass_warning: Option<String>,
}

impl Refusal {
    /// A refusal with no late-bypass warning attached yet (the pipeline adds
    /// it from the context).
    #[must_use]
    pub fn new(guard: GuardKind, message: impl Into<String>) -> Self {
        Self {
            guard,
            message: message.into(),
            late_bypass_warning: None,
        }
    }
}
