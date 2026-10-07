//! The text a check reads.

/// One `bash()` command as the guards see it.
///
/// `command` is what the caller wrote (the display value). `script` is the
/// exact text the shell runs: the trusted setup prefix
/// (`PRIME_AGENT_BASH_COMMAND_PREFIX`, read once by the kernel) on its own
/// line, then the command. `prefix` is that prefix, or `None` when the script
/// has no trusted prefix region (a caller-supplied script is scanned whole as
/// user text, so a prefix boundary cannot hide words).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Script<'a> {
    pub command: &'a str,
    pub script: &'a str,
    pub prefix: Option<&'a str>,
}

impl<'a> Script<'a> {
    /// The script the kernel builds for `command` under `prefix`: the prefix
    /// on its own line when one is set (`_prefix_command`).
    #[must_use]
    pub fn compose(command: &str, prefix: Option<&str>) -> String {
        match prefix {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}\n{command}"),
            Some(_) | None => command.to_string(),
        }
    }

    /// A script that is exactly the command (no prefix).
    #[must_use]
    pub fn bare(command: &'a str) -> Self {
        Self {
            command,
            script: command,
            prefix: None,
        }
    }
}
