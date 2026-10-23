//! Worktree names: validation, the branch and directory a name maps to, and
//! the generated name of an unnamed worktree.

use super::WorktreeError;

/// The longest worktree name (Claude Code's limit).
pub const MAX_NAME_LEN: usize = 64;

/// The branch prefix every managed worktree's branch carries.
pub const BRANCH_PREFIX: &str = "worktree-";

/// Check a worktree name: at most [`MAX_NAME_LEN`] characters, `/`-separated
/// segments of letters, digits, `.`, `_` and `-`, none of them `.`, `..` or a
/// spelling of `.git`.
///
/// # Errors
///
/// [`WorktreeError::InvalidName`] naming the rule the name breaks.
pub fn validate_name(name: &str) -> Result<(), WorktreeError> {
    let invalid = |why: String| Err(WorktreeError::InvalidName(why));
    if name.chars().count() > MAX_NAME_LEN {
        return invalid(format!(
            "must be {MAX_NAME_LEN} characters or fewer (got {})",
            name.chars().count()
        ));
    }
    for segment in name.split('/') {
        if segment == "." || segment == ".." {
            return invalid(format!(
                "\"{name}\" must not contain \".\" or \"..\" path segments"
            ));
        }
        if segment.to_ascii_lowercase().trim_end_matches('.') == ".git" {
            return invalid(format!(
                "\"{name}\": \"{segment}\" is a reserved git directory name"
            ));
        }
        let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        if segment.is_empty() || !segment.chars().all(allowed) {
            return invalid(format!(
                "\"{name}\": each \"/\"-separated segment must be non-empty and contain only \
                 letters, digits, dots, underscores, and dashes"
            ));
        }
    }
    Ok(())
}

/// The name as one path component: `/` becomes `+` (Claude Code's flattening),
/// so `feature/login` lives at `<worktrees>/feature+login`.
#[must_use]
pub fn flatten(name: &str) -> String {
    name.replace('/', "+")
}

/// The branch a managed worktree checks out: `worktree-<flattened name>`.
#[must_use]
pub fn branch_for(name: &str) -> String {
    format!("{BRANCH_PREFIX}{}", flatten(name))
}

/// A fresh name for an unnamed worktree: an adjective, a noun and four hex
/// digits (`quiet-river-3fa1`), always a valid name.
#[must_use]
pub fn random_name() -> String {
    const ADJECTIVES: [&str; 16] = [
        "amber", "brisk", "calm", "deft", "eager", "fond", "gentle", "hardy", "keen", "lucid",
        "merry", "nimble", "quiet", "rapid", "steady", "vivid",
    ];
    const NOUNS: [&str; 16] = [
        "aspen", "brook", "cedar", "delta", "ember", "fjord", "grove", "harbor", "inlet",
        "juniper", "meadow", "orchid", "prairie", "river", "summit", "willow",
    ];
    let mut bytes = [0u8; 4];
    // The OS source does not fail on supported platforms; a zeroed buffer
    // still yields a valid (if predictable) name.
    let _ = getrandom::fill(&mut bytes);
    format!(
        "{}-{}-{:02x}{:02x}",
        ADJECTIVES[usize::from(bytes[0] % 16)],
        NOUNS[usize::from(bytes[1] % 16)],
        bytes[2],
        bytes[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_claude_codes_rules() {
        for good in ["fix-login", "feature/login", "v1.2_rc", "a"] {
            assert!(validate_name(good).is_ok(), "{good} should be valid");
        }
        for bad in [
            "",
            "a//b",
            "../escape",
            "a/./b",
            ".git",
            "x/.GIT..",
            "has space",
            "semi;colon",
            &"x".repeat(MAX_NAME_LEN + 1),
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn a_name_maps_to_one_branch_and_one_directory_component() {
        assert_eq!(flatten("feature/login"), "feature+login");
        assert_eq!(branch_for("feature/login"), "worktree-feature+login");
        assert_eq!(branch_for("fix"), "worktree-fix");
    }

    #[test]
    fn generated_names_are_valid() {
        for _ in 0..32 {
            let name = random_name();
            assert!(validate_name(&name).is_ok(), "{name}");
        }
    }
}
