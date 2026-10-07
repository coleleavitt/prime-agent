//! The guard's refusal messages, byte for byte the kernel's.

/// The bypass variable every message names.
const BYPASS_ENV: &str = "PI_BASH_ALLOW_DESTRUCTIVE_CHMOD";

/// The deepest nesting of command substitutions any scan recurses into.
pub(super) const MAX_SUBSTITUTION_NESTING: usize = 100;

/// Why a wrapper payload (eval, `sh -c`, alias, trap, `env -S`, a fed
/// script) hides shell code the guard must refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PayloadReason {
    RecursiveChmod,
    UnresolvableCommand,
    BashEnv,
    ProcessSubstitution,
    EnvSplitExpansion,
    ShellStartup,
    UnscannedScript,
}

/// The common closing lines: how to run the command intentionally.
fn closing(advice: &str) -> String {
    format!(
        "\n\n{advice} bash(command, allow_destructive_chmod=True), or start the kernel with {BYPASS_ENV}=1."
    )
}

pub(super) fn nesting() -> String {
    format!(
        "Refusing to run this command: its command text nests more than {MAX_SUBSTITUTION_NESTING} levels of command substitution, too deep for the guard to scan.{}",
        closing("Simplify the command, or retry with")
    )
}

pub(super) fn operand(
    operand: Option<&str>,
    resolved: Option<&str>,
    workspace: &str,
    reason: &str,
) -> String {
    let detail = match (operand, resolved) {
        (None, _) => format!("  an operand {reason}."),
        (Some(operand), None) => format!("  the operand \"{operand}\" {reason}."),
        (Some(operand), Some(resolved)) => {
            format!("  the operand \"{operand}\" {reason} ({resolved}).")
        }
    };
    format!(
        "Refusing to run this recursive chmod/chown command:\n{detail}\nRecursive chmod/chown must stay inside the kernel workspace ({workspace}) and must never target the home directory, dot-directories (e.g. .git), dotfiles, or the filesystem root.{}",
        closing("To run it intentionally, retry with")
    )
}

pub(super) fn relocation() -> String {
    format!(
        "Refusing to run this recursive chmod/chown command: it changes directory (or wraps the command in xargs) first, and the directory or targets it would act on cannot be determined safely.{}",
        closing("Run it as its own command from the target directory, or retry with")
    )
}

pub(super) fn eval() -> String {
    format!(
        "Refusing to run this recursive chmod/chown command: it wraps a recursive chmod/chown in eval, and the directories it targets cannot be resolved safely.{}",
        closing("Run it directly, or retry with")
    )
}

pub(super) fn bash_env() -> String {
    format!(
        "Refusing to run this command: it arms BASH_ENV, and bash runs that file's shell code before the command text -- code the guard cannot scan, so a recursive chmod/chown could escape the workspace unseen.{}",
        closing("Run it without BASH_ENV, or retry with")
    )
}

pub(super) fn process_substitution() -> String {
    format!(
        "Refusing to run this command: it feeds a process substitution to a shell wrapper (for example `bash <(...)`), and the wrapper executes that output as shell code that cannot be scanned statically.{}",
        closing("Run it without the process substitution, or retry with")
    )
}

pub(super) fn definition() -> String {
    format!(
        "Refusing to run this recursive chmod/chown command: it defines a shell function (or alias) whose chmod/chown and recursive flag can combine at call time, and the resulting run cannot be resolved statically.{}",
        closing("Write the chmod/chown command literally, or retry with")
    )
}

pub(super) fn wrapper_script() -> String {
    format!(
        "Refusing to run this command: a shell wrapper executes a script from outside the kernel workspace (or a path the guard cannot resolve), and that file's content cannot be scanned statically.{}",
        closing("Run it from inside the workspace, or retry with")
    )
}

pub(super) fn env_split_string() -> String {
    format!(
        "Refusing to run this recursive chmod/chown command: it runs a recursive chmod/chown inside a quoted `env -S` payload whose targets cannot be resolved safely.{}",
        closing("Run it directly, or retry with")
    )
}

pub(super) fn env_split_string_expansion() -> String {
    format!(
        "Refusing to run this recursive chmod/chown command: its `env -S` string builds the argv env runs through shell expansion, which the guard cannot resolve.{}",
        closing("Write the command literally, or retry with")
    )
}

pub(super) fn shell_startup() -> String {
    format!(
        "Refusing to run this command: it starts a login or interactive shell, which sources profile and rc files before it runs the command it was given, and startup code cannot be scanned\nstatically.{}",
        closing("Run the command in a non-interactive, non-login shell (`bash -c ...`), or retry with")
    )
}

pub(super) fn hash_alias() -> String {
    format!(
        "Refusing to run this command: it installs a command-hash entry (`hash -p`) whose target the guard cannot resolve, so a later command word could run a recursive chmod/chown the scanner never sees.{}",
        closing("Register the command with a literal path, or retry with")
    )
}

pub(super) fn shadowed_command() -> String {
    format!(
        "Refusing to run this recursive chmod/chown command: a PATH assignment here makes the shell search a relative directory for the command word, so a file named chmod/chown in the workspace could run instead and act on targets the guard never checked.{}",
        closing("Run it with an absolute command path and an absolute PATH, or retry with")
    )
}

pub(super) fn trap() -> String {
    format!(
        "Refusing to run this recursive chmod/chown command: it installs a trap whose body runs a recursive chmod/chown the guard cannot resolve safely.{}",
        closing("Run it directly, or retry with")
    )
}

pub(super) fn pipe_fed_wrapper() -> String {
    format!(
        "Refusing to run this command: a bare shell wrapper reads its commands from a pipe (or here-string/redirect) whose content cannot be scanned statically.{}",
        closing("Run the commands directly, or retry with")
    )
}

pub(super) fn unresolvable_command() -> String {
    format!(
        "Refusing to run this recursive chmod/chown command: its command name cannot be determined statically because it is built from a variable or a substitution (directly, or behind xargs/sudo/env and similar wrappers); a recursive flag is present, so the run is refused rather than guessed at.{}",
        closing("Write the chmod/chown command literally, or retry with")
    )
}

pub(super) fn shell_c() -> String {
    format!(
        "Refusing to run this recursive chmod/chown command: it runs a recursive chmod/chown inside a quoted `sh -c` payload whose targets cannot be resolved safely.{}",
        closing("Run it directly, or retry with")
    )
}

/// The message for a payload-hidden reason other than a plain recursion
/// (`_payload_reason_message`).
pub(super) fn payload_reason(reason: PayloadReason) -> String {
    match reason {
        PayloadReason::BashEnv => bash_env(),
        PayloadReason::UnresolvableCommand => unresolvable_command(),
        PayloadReason::UnscannedScript => wrapper_script(),
        PayloadReason::ShellStartup => shell_startup(),
        PayloadReason::EnvSplitExpansion => env_split_string_expansion(),
        PayloadReason::RecursiveChmod | PayloadReason::ProcessSubstitution => {
            process_substitution()
        }
    }
}
