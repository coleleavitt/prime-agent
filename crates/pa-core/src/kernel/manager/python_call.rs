//! One-shot Python processes beside the kernel: the factory's python
//! states. Each call starts the kernel's interpreter with the kernel's
//! environment, in its working directory, under the OS sandbox the
//! kernel's `bash()` commands run under, as the call ran from inside the
//! kernel before the factory moved host-side.

use super::ReplKernelManager;
use super::startup::kernel_spawn_env;

impl ReplKernelManager {
    /// A `Command` for this kernel's interpreter, its environment (without
    /// the `RLM_*` variables that identify the kernel session), its
    /// working directory, and its OS sandbox. The caller adds the
    /// arguments and stdio.
    ///
    /// # Errors
    ///
    /// The sentence a caller reports when no interpreter is resolved yet,
    /// or when the session's sandbox cannot be enforced on this machine.
    pub fn python_command(&self) -> Result<std::process::Command, String> {
        let python = self
            .inner
            .options
            .python
            .clone()
            .or_else(|| self.resolved_python())
            .ok_or_else(|| "the session's Python kernel has not started".to_string())?;
        let mut command = match self.inner.bash_jobs.sandbox() {
            pa_bash::JobSandbox::Unconfined => std::process::Command::new(&python),
            pa_bash::JobSandbox::Confined(prepared) => prepared.command(&python),
            pa_bash::JobSandbox::Unavailable(reason) => return Err(reason),
        };
        let env = kernel_spawn_env(&self.inner.options.env, self.inner.options.environment);
        command.env_clear().envs(
            env.into_iter()
                .filter(|(key, _)| !key.to_string_lossy().starts_with("RLM_")),
        );
        if let Some(cwd) = self.inner.start_cwd() {
            command.current_dir(cwd);
        }
        Ok(command)
    }
}
