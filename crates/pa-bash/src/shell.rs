//! The shell a kernel command runs in and the environment it gets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::context::GuardContext;
use crate::verdict::GuardKind;

/// The kernel names its shell through this variable (the host injects an
/// absolute path when it finds one).
pub(crate) const SHELL_ENV: &str = "PRIME_AGENT_BASH_SHELL";

/// Why no shell could be chosen. The kernel raises the first as `ValueError`
/// and the second as `RuntimeError`, with these messages.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShellError {
    #[error("PRIME_AGENT_BASH_SHELL must be an absolute path")]
    NotAbsolute,
    #[error(
        "bash() needs PRIME_AGENT_BASH_SHELL set to the absolute path of a POSIX shell on \
         Windows (e.g. install Git Bash in its default location so the host injects it)"
    )]
    NoWindowsShell,
}

/// The shell for one command, read per call so an environment change made in
/// the kernel applies to later commands. Windows never consults `PATH` (a
/// repository-controlled `PATH` could supply the shell); elsewhere the `PATH`
/// fallback serves standalone use, since the host always injects the variable.
///
/// # Errors
///
/// [`ShellError`] when the variable is relative, or unset on Windows.
pub fn resolve_shell(context: &GuardContext) -> Result<PathBuf, ShellError> {
    if let Some(shell) = context.var(SHELL_ENV).filter(|value| !value.is_empty()) {
        let path = Path::new(shell);
        if !is_absolute(shell) {
            return Err(ShellError::NotAbsolute);
        }
        return Ok(path.to_path_buf());
    }
    if cfg!(windows) {
        return Err(ShellError::NoWindowsShell);
    }
    Ok(which("bash", context.var("PATH")).unwrap_or_else(|| PathBuf::from("/bin/sh")))
}

/// `os.path.isabs` for the platform the kernel runs on.
fn is_absolute(path: &str) -> bool {
    Path::new(path).is_absolute() || (cfg!(windows) && path.starts_with(['/', '\\']))
}

/// `shutil.which(name, path=search)`: the first executable regular file named
/// `name` on the search path.
pub(crate) fn which(name: &str, search: Option<&str>) -> Option<PathBuf> {
    let search = search?;
    std::env::split_paths(search)
        .map(|dir| {
            if dir.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                dir
            }
        })
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Variables every kernel command gets: agent commands have no usable stdin,
/// so interactive prompts (an editor for `git commit`, credential asks,
/// pagers) can only hang. Inline assignments (`GIT_EDITOR=vim git commit`)
/// still win, since they replace the exported value for that command.
const NON_INTERACTIVE: [(&str, &str); 14] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("CLICOLOR", "0"),
    ("FORCE_COLOR", "0"),
    ("GIT_EDITOR", "true"),
    ("GIT_SEQUENCE_EDITOR", "true"),
    ("GIT_TERMINAL_PROMPTS", "0"),
    ("GIT_ASKPASS", "true"),
    ("SSH_ASKPASS_REQUIRE", "never"),
    ("EDITOR", "true"),
    ("VISUAL", "true"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("DEBIAN_FRONTEND", "noninteractive"),
];

/// The W3C trace-context variable child processes read.
pub(crate) const TRACEPARENT_ENV: &str = "TRACEPARENT";

/// The environment of a kernel-spawned command (and of the guards' probes):
/// the kernel's own environment plus the non-interactive settings, with the
/// guard bypass variables dropped unless the kernel was launched with them
/// (a mid-session write must not arm a nested kernel's frozen snapshot), and
/// `BASH_ENV`, `ENV` and every exported `BASH_FUNC_name%%` function dropped
/// (an unscanned startup file or a function shadowing a command word the
/// guards read literally). `TRACEPARENT` carries the context's trace span.
#[must_use]
pub fn child_env(context: &GuardContext) -> BTreeMap<String, String> {
    let mut env = context.env().clone();
    for (name, value) in NON_INTERACTIVE {
        env.insert(name.to_string(), value.to_string());
    }
    for guard in GuardKind::ALL {
        if !context.launch_bypassed(guard) {
            env.remove(guard.bypass_env());
        }
    }
    env.remove("BASH_ENV");
    env.remove("ENV");
    env.retain(|name, _| !name.starts_with("BASH_FUNC_"));
    if let Some(traceparent) = context.traceparent() {
        env.insert(TRACEPARENT_ENV.to_string(), traceparent.to_string());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(env: &[(&str, &str)]) -> GuardContext {
        GuardContext::new(
            "/",
            env.iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
        )
    }

    #[test]
    fn child_env_scrubs_startup_hooks_and_unlaunched_bypasses() {
        let ctx = context(&[
            ("KEEP", "1"),
            ("BASH_ENV", "/tmp/rc"),
            ("ENV", "/tmp/rc"),
            ("BASH_FUNC_rm%%", "() { :; }"),
            ("PI_BASH_ALLOW_SUDO", "1"),
            ("PI_BASH_ALLOW_FORCE_PUSH", "1"),
            ("PAGER", "less"),
        ])
        .with_launch_bypass(GuardKind::ForcePush)
        .with_traceparent(Some("00-aa-bb-01".into()));
        let env = child_env(&ctx);
        let mut expected: BTreeMap<String, String> = NON_INTERACTIVE
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        expected.insert("KEEP".into(), "1".into());
        expected.insert("PI_BASH_ALLOW_FORCE_PUSH".into(), "1".into());
        expected.insert("TRACEPARENT".into(), "00-aa-bb-01".into());
        assert_eq!(env, expected);
    }

    #[test]
    fn a_relative_shell_override_is_refused() {
        assert_eq!(
            resolve_shell(&context(&[("PRIME_AGENT_BASH_SHELL", "bash")])),
            Err(ShellError::NotAbsolute)
        );
        assert_eq!(
            resolve_shell(&context(&[("PRIME_AGENT_BASH_SHELL", "/bin/dash")])),
            Ok(PathBuf::from("/bin/dash"))
        );
    }
}
