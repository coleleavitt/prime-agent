//! The OS sandbox a kernel's commands run under.
//!
//! Before the port, `bash()` spawned its commands from the kernel process,
//! which the session's sandbox confines, so every command inherited the
//! policy. The host now spawns them, so it applies the same prepared
//! restriction to every process it starts for the kernel: the commands, the
//! guards' probes, and the helpers the journal runs.

use std::ffi::OsStr;
use std::process::Command;
use std::sync::Arc;

use pa_os_sandbox::PreparedSandbox;

/// How a kernel's commands are confined.
#[derive(Debug, Clone, Default)]
pub enum JobSandbox {
    /// No OS sandbox (the `sandbox` setting is off). Processes still inherit
    /// whatever confinement the spawning process itself runs under.
    #[default]
    Unconfined,
    /// Every process starts under this restriction (the kernel's own).
    Confined(Arc<PreparedSandbox>),
    /// The setting asks for confinement this machine cannot enforce: nothing
    /// may start (the reason is the refusal).
    Unavailable(String),
}

impl JobSandbox {
    /// A `Command` running `program` under this sandbox.
    ///
    /// # Errors
    ///
    /// The refusal, when the sandbox is [`JobSandbox::Unavailable`].
    pub(crate) fn command(&self, program: impl AsRef<OsStr>) -> Result<Command, String> {
        match self {
            JobSandbox::Unconfined => Ok(Command::new(program)),
            JobSandbox::Confined(prepared) => Ok(prepared.command(program)),
            JobSandbox::Unavailable(reason) => Err(reason.clone()),
        }
    }
}
