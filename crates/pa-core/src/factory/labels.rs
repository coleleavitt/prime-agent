//! Spawn labels: the sibling names a factory run's children carry.
//!
//! The supervisor requires unique sibling names (capped at 64 characters),
//! and one state's settled children stay registered for the run's life, so
//! every admission — re-entry, foreach fan-out, retry — needs a fresh,
//! readable, bounded label. The validator shares [`suffixed_spawn_form`] to
//! reject configured names that would collide with another state's
//! suffixed labels at write time.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// The host's cap on one spawn name; a configured inline subagent name is
/// bounded by it at write time.
pub const SUBAGENT_NAME_MAX_LENGTH: usize = 64;

/// The first `len` hex characters of the full text's SHA-256.
fn digest_prefix(text: &str, len: usize) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex.truncate(len);
    hex
}

/// The first `count` characters (code points, like a Python slice).
fn char_prefix(text: &str, count: usize) -> &str {
    match text.char_indices().nth(count) {
        Some((offset, _)) => &text[..offset],
        None => text,
    }
}

/// The generated sibling name for one spawned instance:
/// `sw-<state token>-<run prefix>[-i<instance>][-a<attempt>]`. A state id
/// longer than 20 characters is truncated and disambiguated with a 64-bit
/// digest of the full id, so two states sharing a prefix never collide and
/// the longest label stays under the host's cap.
#[must_use]
pub fn child_name(run_id: &str, state_id: &str, instance_index: i64, attempt: u32) -> String {
    let token = if state_id.chars().count() <= 20 {
        state_id.to_string()
    } else {
        format!(
            "{}-{}",
            char_prefix(state_id, 20),
            digest_prefix(state_id, 16)
        )
    };
    let mut parts = vec!["sw".to_string(), token, char_prefix(run_id, 6).to_string()];
    if instance_index >= 0 {
        parts.push(format!("i{instance_index}"));
    }
    if attempt > 1 {
        parts.push(format!("a{attempt}"));
    }
    parts.join("-")
}

/// The sibling label for one spawned instance: the state's configured
/// inline subagent name when it has one (verbatim for the first instance
/// on its first attempt — agents message the child by exactly this label),
/// else the generated [`child_name`]. Later instances add `-i<n>`, retries
/// `-a<n>`; a suffixed label that would exceed the cap shrinks its base to
/// a digest-suffixed token of the full name, so distinct names stay
/// distinct and every admission fits.
#[must_use]
pub fn spawn_label(
    configured: Option<&str>,
    run_id: &str,
    state_id: &str,
    instance_index: i64,
    attempt: u32,
) -> String {
    let Some(base) = configured else {
        return child_name(run_id, state_id, instance_index, attempt);
    };
    let mut suffix = String::new();
    if instance_index > 0 {
        let _ = write!(suffix, "-i{instance_index}");
    }
    if attempt > 1 {
        let _ = write!(suffix, "-a{attempt}");
    }
    let base_len = base.chars().count();
    let suffix_len = suffix.chars().count();
    if base_len + suffix_len > SUBAGENT_NAME_MAX_LENGTH {
        let digest = digest_prefix(base, 16);
        let room = SUBAGENT_NAME_MAX_LENGTH.saturating_sub(suffix_len + digest.len() + 1);
        return format!("{}-{digest}{suffix}", char_prefix(base, room));
    }
    format!("{base}{suffix}")
}

/// Whether `candidate` is a spawn label `base` can produce: `base-i<n>`
/// (n >= 1) and `base-a<n>` (n >= 2) segments, in any combination. The
/// never-generated `-i0` and `-a1` do not count.
#[must_use]
pub fn suffixed_spawn_form(base: &str, candidate: &str) -> bool {
    let Some(remainder) = candidate
        .strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    for part in remainder.split('-') {
        let mut chars = part.chars();
        let Some(kind) = chars.next() else {
            return false;
        };
        let digits = chars.as_str();
        if digits.is_empty()
            || !(kind == 'i' || kind == 'a')
            || !digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            return false;
        }
        // Digit runs compare by value; a run too long for u128 is huge.
        let value = digits.parse::<u128>().unwrap_or(u128::MAX);
        if (kind == 'i' && value < 1) || (kind == 'a' && value < 2) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ports of `test_factory.py` ChildNameTest / SpawnLabelTest: the label
    // helpers moved here from the kernel (`_child_name`/`_spawn_label`).

    #[test]
    fn short_state_ids_keep_their_slug() {
        let name = child_name("0123456789abcdef", "collect", 0, 1);
        assert_eq!(name, "sw-collect-012345-i0");
        assert!(name.len() <= 64);
    }

    #[test]
    fn long_state_ids_disambiguate_with_a_digest() {
        let one = child_name("0123456789abcdef", "collect-findings-pass-one", 0, 1);
        let two = child_name("0123456789abcdef", "collect-findings-pass-two", 0, 1);
        assert_ne!(one, two);
        assert!(one.len() <= 64 && two.len() <= 64);
        assert_ne!(
            child_name("r", "collect", 0, 1),
            child_name("r", "collect", 1, 1)
        );
        assert!(child_name("r", "collect", 0, 2).ends_with("-a2"));
    }

    #[test]
    fn configured_name_labels_the_first_instance_verbatim() {
        assert_eq!(
            spawn_label(Some("reviewer"), "0123456789abcdef", "reviewing", 0, 1),
            "reviewer"
        );
    }

    #[test]
    fn configured_name_keeps_the_generated_suffixes_when_disambiguating() {
        let labels = [
            spawn_label(Some("reviewer"), "r", "reviewing", 1, 1),
            spawn_label(Some("reviewer"), "r", "reviewing", 0, 2),
            spawn_label(Some("reviewer"), "r", "reviewing", 2, 3),
        ];
        assert_eq!(labels, ["reviewer-i1", "reviewer-a2", "reviewer-i2-a3"]);
        assert_ne!(
            spawn_label(Some("reviewer"), "r", "reviewing", 0, 1),
            spawn_label(Some("reviewer"), "r", "reviewing", 1, 1)
        );
    }

    #[test]
    fn absent_configured_name_falls_back_to_the_generated_label() {
        assert_eq!(
            spawn_label(None, "0123456789abcdef", "collect", 0, 1),
            child_name("0123456789abcdef", "collect", 0, 1)
        );
    }

    #[test]
    fn suffixed_labels_stay_within_the_host_cap() {
        let long_name = "x".repeat(SUBAGENT_NAME_MAX_LENGTH);
        assert_eq!(spawn_label(Some(&long_name), "r", "a", 0, 1), long_name);
        let second = spawn_label(Some(&long_name), "r", "a", 1, 1);
        assert!(second.len() <= SUBAGENT_NAME_MAX_LENGTH);
        assert!(second.ends_with("-i1"));
        assert!(second.contains(&digest_prefix(&long_name, 16)));
        let sharing_prefix = "x".repeat(SUBAGENT_NAME_MAX_LENGTH - 1) + "y";
        let other = spawn_label(Some(&sharing_prefix), "r", "a", 1, 1);
        assert_ne!(second, other);
        assert!(other.len() <= SUBAGENT_NAME_MAX_LENGTH);
        for (instance_index, attempt) in [(0, 2), (9, 12), (1_000_000, 99)] {
            let label = spawn_label(Some(&long_name), "r", "a", instance_index, attempt);
            assert!(label.len() <= SUBAGENT_NAME_MAX_LENGTH, "{label}");
        }
    }

    #[test]
    fn suffixed_forms_match_generated_labels_only() {
        let verdicts = [
            suffixed_spawn_form("foo", "foo-i1"),
            suffixed_spawn_form("foo", "foo-a2"),
            suffixed_spawn_form("foo", "foo-i3-a4"),
            suffixed_spawn_form("foo", "foo-i0"),
            suffixed_spawn_form("foo", "foo-a1"),
            suffixed_spawn_form("foo", "foo-bar"),
            suffixed_spawn_form("foo", "foo"),
            suffixed_spawn_form("foo", "foo-"),
            suffixed_spawn_form("foo", "foobar-i1"),
        ];
        assert_eq!(
            verdicts,
            [true, true, true, false, false, false, false, false, false]
        );
    }
}
