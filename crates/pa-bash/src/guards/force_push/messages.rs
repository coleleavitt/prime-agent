//! The refusal messages, byte for byte the kernel's.

const BYPASS_ENV: &str = "PI_BASH_ALLOW_FORCE_PUSH";

/// The one-time stderr warning when the bypass variable appeared mid-session.
pub(super) const LATE_BYPASS_WARNING: &str =
    "prime-agent bash: PI_BASH_ALLOW_FORCE_PUSH appeared after kernel start \
     and is ignored; the force-push guard only honors it when the kernel is started with it set.";

/// The common refusal around one reason phrase.
pub(super) fn refusal(reason: &str) -> String {
    format!(
        "Refusing to run this force-push command: {reason}.\n\nForce-pushes rewrite remote history; a force-push \
         to main/master or the current upstream can discard other people's work in one step.\n\nUse \
         --force-with-lease instead: it refuses to overwrite unless the remote ref still matches what you \
         have.\n\nTo force-push anyway, retry with bash(command, allow_force_push=True), or start the kernel \
         with {BYPASS_ENV}=1."
    )
}

/// A refusal with its own lead sentence and advice.
fn standalone(lead: &str, advice: &str) -> String {
    format!(
        "Refusing to run this force-push command: {lead}\n\n{advice} bash(command, allow_force_push=True), or \
         start the kernel with {BYPASS_ENV}=1."
    )
}

pub(super) fn relocation_refusal() -> String {
    standalone(
        "it changes directory (or relocates the repository) first, and the branch it would rewrite cannot be \
         determined safely.",
        "Run it as its own command from the target directory, or retry with",
    )
}

pub(super) fn eval_refusal() -> String {
    standalone(
        "it wraps a force-push in eval, and the target it would rewrite cannot be resolved safely.",
        "Run it directly, or retry with",
    )
}

pub(super) fn nesting_refusal() -> String {
    standalone(
        &format!(
            "its command substitutions nest more than {} levels deep, which the guard does not follow, so what \
             it runs cannot be verified.",
            super::budget::MAX_SUBSTITUTION_DEPTH
        ),
        "Flatten the substitutions (or run the inner command directly), or retry with",
    )
}

pub(super) fn scan_refusal() -> String {
    standalone(
        "its command substitutions nest too deeply for the guard's scan budget, so the guard cannot verify \
         what it would run.",
        "Flatten the substitutions (or run the inner command directly), or retry with",
    )
}

pub(super) fn shell_c_refusal() -> String {
    standalone(
        "it runs a force-push inside a quoted `sh -c` payload whose target cannot be resolved safely.",
        "Run it directly, or retry with",
    )
}

pub(super) fn env_refusal() -> String {
    standalone(
        "it runs a force-push inside an `env -S`/`--split-string` payload whose target cannot be resolved \
         safely.",
        "Run it directly, or retry with",
    )
}

pub(super) fn alias_refusal() -> String {
    standalone(
        "it defines a git alias (`-c alias.X=...`) for the subcommand it invokes, and the argv that alias \
         expands to cannot be resolved safely.",
        "Run the push directly with the aliased name spelled out, or retry with",
    )
}

pub(super) fn probe_timeout_refusal() -> String {
    standalone(
        "resolving the branch it would rewrite timed out (a `git rev-parse` probe the guard runs before \
         spawning anything), so the target cannot be determined safely.",
        "Retry the command, or retry with",
    )
}

pub(super) fn git_subcommand_refusal(subcommand: &str) -> String {
    format!(
        "Refusing to run this git command: `{subcommand}` is outside the git command set this guard was \
         calibrated against (Apple git 2.50.1 and Homebrew git 2.55.0), so it is a repository or user alias, or \
         an external `git-` program, or a command only a newer git knows: the guard cannot verify what it runs. \
         An alias can force-push a protected branch, which is why an unknown name is refused even when it looks \
         harmless.\n\nSpell out the real subcommand, or run the underlying program (for `{subcommand}`) \
         directly. To run this command as written, retry with bash(command, allow_force_push=True), or start the \
         kernel with {BYPASS_ENV}=1."
    )
}

pub(super) fn env_option_refusal(option: &str) -> String {
    refusal(&format!(
        "the env option \"{option}\" is an abbreviation that matches more than one of env's long options, so \
         the guard cannot tell whether it takes a value and which word it hands env"
    ))
}

pub(super) fn config_option_refusal(operand: &str) -> String {
    refusal(&format!(
        "its inline config operand \"{operand}\" cannot be read statically, so the configuration it applies -- \
         which can arm a force push through remote.<name>.push or remote.<name>.mirror -- cannot be checked"
    ))
}

pub(super) fn mirror_config_refusal() -> String {
    refusal(
        "a remote.<name>.mirror or remote.<name>.push setting in this command can turn the push into a forced \
         one the guard cannot verify (a mirror remote force-updates every ref; a configured push refspec can \
         carry a +)",
    )
}
