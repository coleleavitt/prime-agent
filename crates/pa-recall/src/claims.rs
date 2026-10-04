//! Build claims: "this command exited 0 on exactly this workspace". A claim
//! is never served as an answer. It is rendered CURRENT only while the
//! workspace digest it was recorded against still matches a fresh
//! recomputation, and EXPIRED with a reason otherwise.

use std::sync::LazyLock;

use fancy_regex::Regex;
use serde_json::{json, Value};

use crate::time::parse_iso_millis;

/// Claims kept per repo mark.
pub const RECALL_MAX_CLAIMS: usize = 8;
/// Longest command (UTF-16 units, as the TS product counts) a claim keeps.
pub const RECALL_MAX_CLAIM_COMMAND_CHARS: usize = 500;

/// One build claim as the mark file stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallClaim {
    pub command: String,
    pub exit_code: i64,
    /// ISO timestamp the claim was recorded.
    pub at: String,
    /// `workspace_digest` of the workspace the command ran against.
    pub digest_at_claim: String,
}

impl RecallClaim {
    /// The mark-file form, keys in the TS product's order.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "command": self.command,
            "exitCode": self.exit_code,
            "at": self.at,
            "digestAtClaim": self.digest_at_claim,
        })
    }

    /// A claim read from a mark file; `None` for anything that is not one
    /// (TS `isRecallClaim`).
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        let exit_code = value.get("exitCode")?;
        // JS `Number.isInteger`: an integral float such as `0.0` counts.
        #[allow(clippy::cast_possible_truncation)] // integral and within ±2^53, so exact
        let exit_code = exit_code.as_i64().or_else(|| {
            exit_code
                .as_f64()
                .filter(|code| code.fract() == 0.0 && code.abs() < 9_007_199_254_740_992.0)
                .map(|code| code as i64)
        })?;
        Some(Self {
            command: value.get("command")?.as_str()?.to_string(),
            exit_code,
            at: value.get("at")?.as_str()?.to_string(),
            digest_at_claim: value.get("digestAtClaim")?.as_str()?.to_string(),
        })
    }
}

/// Whether a claim still holds against the live workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimStatus {
    Current,
    Expired { reason: String },
}

/// A claim and its status against the live workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimVerdict {
    pub claim: RecallClaim,
    pub status: ClaimStatus,
}

const RUNNER: &str = r"(?:(?:npx|pnpm\s+exec|pnpm\s+dlx|yarn|bunx)\s+)?";

static BUILD_SEGMENT_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        format!(r"^{RUNNER}(?:tsgo|tsc|vue-tsc)(?:\s|$)"),
        format!(r"^{RUNNER}(?:@biomejs/)?biome\s+(?:check|ci|lint)(?:\s|$)"),
        r"^cargo\s+(?:\+\S+\s+)?(?:build|test|check|clippy|nextest\s+run)(?:\s|$)".to_string(),
        r"^(?:npm|pnpm|yarn|bun)\s+(?:test|t)(?:\s|$)".to_string(),
        r"^(?:npm|pnpm|yarn|bun)\s+run\s+(?:build|check|test|typecheck|lint)(?:[:\s]|$)"
            .to_string(),
        r"^(?:pnpm|yarn)\s+(?:build|check|typecheck)(?:\s|$)".to_string(),
        r"^(?:(?:python3?|uv\s+run|poetry\s+run)\s+(?:-m\s+)?)?pytest(?:\s|$)".to_string(),
        r"^go\s+(?:build|test|vet)(?:\s|$)".to_string(),
        r"^make(?:\s|$)".to_string(),
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("build segment pattern compiles"))
    .collect()
});

static BUILD_COMMAND_MENTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(?:tsgo|tsc|pytest|make)\b|\bcargo\s+(?:\+\S+\s+)?(?:build|test|check|clippy|nextest)\b|\b(?:npm|pnpm|yarn|bun)\s+(?:test|t|run\s+(?:build|check|test|typecheck|lint))\b|\b(?:pnpm|yarn)\s+(?:build|check|typecheck)\b|\bgo\s+(?:build|test|vet)\b|\bbiome\s+(?:check|ci|lint)\b",
    )
    .expect("build mention pattern compiles")
});

/// Shell constructs that can report 0 while the build failed.
static UNSAFE_SHELL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\n\r;`]|\$\(|\|\||(?<![&>])&(?![&>])|\|").expect("unsafe shell pattern compiles")
});
static REDIRECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+(?:\d?>>?|&>)\s*\S+").expect("redirection pattern compiles"));
static ENV_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=\S*$").expect("env assignment pattern compiles")
});
static CD_SEGMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^cd\s+\S+$").expect("cd pattern compiles"));

fn matches(regex: &Regex, text: &str) -> bool {
    regex.is_match(text).unwrap_or(false)
}

/// True when cell source names a build or test command anywhere. A cheap
/// pre-filter: only such cells pay for a workspace digest before they run.
#[must_use]
pub fn mentions_build_command(source: &str) -> bool {
    matches(&BUILD_COMMAND_MENTION, source)
}

fn strip_env_assignments(segment: &str) -> String {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let skip = words
        .iter()
        .take_while(|word| matches(&ENV_ASSIGNMENT, word))
        .count();
    words[skip..].join(" ")
}

/// True for a command whose exit code 0 is evidence the build or tests
/// passed. `&&` chains of build commands and `cd` qualify; pipes, `||`,
/// `;`, background jobs, and substitutions do not, because each can report
/// 0 when the build failed.
#[must_use]
pub fn is_build_claim_command(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() || utf16_len(trimmed) > RECALL_MAX_CLAIM_COMMAND_CHARS {
        return false;
    }
    if matches(&UNSAFE_SHELL, trimmed) {
        return false;
    }
    let mut saw_build = false;
    for raw_segment in trimmed.split("&&") {
        let without_redirects = REDIRECTION.replace_all(raw_segment.trim(), "");
        let segment = strip_env_assignments(&without_redirects);
        if segment.is_empty() {
            return false;
        }
        if matches(&CD_SEGMENT, &segment) {
            continue;
        }
        if !BUILD_SEGMENT_PATTERNS
            .iter()
            .any(|pattern| matches(pattern, &segment))
        {
            return false;
        }
        saw_build = true;
    }
    saw_build
}

fn claim_time(claim: &RecallClaim) -> i64 {
    parse_iso_millis(&claim.at).unwrap_or(0)
}

/// Existing claims plus new ones, one per command (the newest wins),
/// bounded to the [`RECALL_MAX_CLAIMS`] newest.
#[must_use]
pub fn merge_recall_claims(existing: &[RecallClaim], added: &[RecallClaim]) -> Vec<RecallClaim> {
    // Insertion-ordered by first sighting, like a JS Map.
    let mut by_command: Vec<RecallClaim> = Vec::new();
    for claim in existing.iter().chain(added) {
        match by_command
            .iter_mut()
            .find(|held| held.command == claim.command)
        {
            Some(held) => {
                if claim_time(claim) >= claim_time(held) {
                    *held = claim.clone();
                }
            }
            None => by_command.push(claim.clone()),
        }
    }
    by_command.sort_by_key(claim_time);
    let excess = by_command.len().saturating_sub(RECALL_MAX_CLAIMS);
    by_command.split_off(excess)
}

/// String length in UTF-16 code units, the TS product's `length`.
pub(crate) fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The first `max` UTF-16 code units of `text` (TS `slice(0, max)`), never
/// splitting a character.
pub(crate) fn utf16_prefix(text: &str, max: usize) -> &str {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > max {
            return &text[..index];
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_claim_commands_are_only_chains_whose_zero_proves_the_build() {
        for command in [
            "npx tsgo --noEmit",
            "cd packages/coding-agent && npx tsgo --noEmit",
            "cargo test -p core",
            "cargo +1.98.1 clippy --workspace",
            "npm run check > /dev/null 2>&1",
            "CI=1 pytest -q",
            "uv run pytest",
            "make",
        ] {
            assert!(is_build_claim_command(command), "{command}");
        }
        for command in [
            "npm test | tail -5",
            "npm test || true",
            "make; rm -rf build",
            "npx tsgo --noEmit &",
            "git status",
            "cd pkg",
            "echo $(make)",
            "",
        ] {
            assert!(!is_build_claim_command(command), "{command}");
        }
        assert!(!is_build_claim_command(&format!(
            "make {}",
            "x".repeat(500)
        )));
    }

    #[test]
    fn a_cell_mentions_a_build_command_anywhere_in_its_source() {
        assert!(mentions_build_command(
            r#"r = await bash("cd pkg && npx tsgo --noEmit")"#
        ));
        assert!(mentions_build_command("await bash('pnpm run build')"));
        assert!(mentions_build_command("await bash('go test ./...')"));
        assert!(!mentions_build_command(
            "await bash('git status')\nprint(make_report())"
        ));
    }

    #[test]
    fn merging_keeps_one_claim_per_command_and_the_newest_eight() {
        let claims: Vec<RecallClaim> = (0..12)
            .map(|index| RecallClaim {
                command: format!("make target-{index}"),
                exit_code: 0,
                at: format!("2026-01-01T00:{index:02}:00.000Z"),
                digest_at_claim: "d".to_string(),
            })
            .collect();
        let renewed = RecallClaim {
            at: "2026-02-01T00:00:00.000Z".to_string(),
            ..claims[0].clone()
        };
        let mut added = claims[6..].to_vec();
        added.push(renewed.clone());
        let merged = merge_recall_claims(&claims[..6], &added);
        let mut expected = claims[5..].to_vec();
        expected.push(renewed);
        assert_eq!(merged, expected);
    }

    #[test]
    fn claims_round_trip_through_the_mark_form() {
        let claim = RecallClaim {
            command: "make".to_string(),
            exit_code: 0,
            at: "2026-01-01T00:00:00.000Z".to_string(),
            digest_at_claim: "d".to_string(),
        };
        assert_eq!(RecallClaim::from_json(&claim.to_json()), Some(claim));
        assert_eq!(
            RecallClaim::from_json(
                &json!({ "command": "make", "exitCode": 0.5, "at": "x", "digestAtClaim": "d" })
            ),
            None
        );
    }
}
