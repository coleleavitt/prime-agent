//! The retained-host capability profile (TS `workflow-v2-capability.ts`,
//! `WORKFLOW-V2.md` §8): the one exact vector a host must advertise, and
//! negotiation — which stays unavailable until the retained host ABI and
//! the controller exist (§12 slice 9).

use serde_json::{Value, json};

use super::wire::{Def, decode_as};

/// The mandatory feature set, in the contract's order.
pub const FEATURES: [&str; 7] = [
    "durable_request_id",
    "direct_parent_ownership",
    "per_turn_settlement",
    "cursor_replay",
    "cancel_fence",
    "tombstone_delete",
    "host_result_attribution",
];

/// The exact normative capability vector.
#[must_use]
pub fn required_capability() -> Value {
    json!({
        "protocol": "prime.workflow.capability/v2",
        "api": "prime.workflow.retained",
        "version": 2,
        "semantics": "2026-09-14",
        "features": FEATURES,
        "limits": {
            "maxPromptUtf8Bytes": 65_536,
            "maxResultUtf8Bytes": 262_144,
            "maxPageSize": 500,
            "maxWaitMs": 30_000,
            "maxChildren": 10_000
        }
    })
}

/// Whether `candidate` is the complete normative vector, compared as one
/// decision: the closed schema pins every constant, the exact feature set
/// (seven unique members, each present), and every limit.
#[must_use]
pub fn matches_profile(candidate: &Value) -> bool {
    decode_as(candidate, Def::Capability).is_ok()
}

/// Why negotiation did not enable V2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Negotiation {
    /// `CAPABILITY_UNAVAILABLE`: no retained host ABI is released, so no
    /// candidate — not even the exact vector — enables the profile.
    Unavailable,
}

/// Negotiate the retained-host capability. Deliberately unavailable: the
/// controller, store, and retained host it would enable are not built.
#[must_use]
pub fn negotiate(_candidate: &Value) -> Negotiation {
    Negotiation::Unavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_complete_vector_matches() {
        assert!(matches_profile(&required_capability()));
        // Feature order is not part of the vector.
        let mut reordered = required_capability();
        reordered["features"].as_array_mut().unwrap().reverse();
        assert!(matches_profile(&reordered));
    }

    #[test]
    fn mixed_version_and_profile_vectors_fail_atomically() {
        let mut version = required_capability();
        version["version"] = json!(1);
        assert!(!matches_profile(&version));
        let mut protocol = required_capability();
        protocol["protocol"] = json!("prime.workflow.capability/v1");
        assert!(!matches_profile(&protocol));
    }

    #[test]
    fn a_missing_substituted_or_duplicated_feature_or_limit_fails() {
        let mut missing = required_capability();
        missing["features"]
            .as_array_mut()
            .unwrap()
            .retain(|feature| feature != "cancel_fence");
        assert!(!matches_profile(&missing));
        let mut substituted = required_capability();
        substituted["features"][0] = json!("future_feature");
        assert!(!matches_profile(&substituted));
        let mut duplicated = required_capability();
        duplicated["features"][0] = json!("cancel_fence");
        assert!(!matches_profile(&duplicated));
        let mut limit = required_capability();
        limit["limits"]["maxWaitMs"] = json!(30_001);
        assert!(!matches_profile(&limit));
        let mut extra = required_capability();
        extra["limits"]["maxDepth"] = json!(1);
        assert!(!matches_profile(&extra));
    }

    #[test]
    fn negotiation_stays_unavailable_even_for_the_exact_vector() {
        assert_eq!(negotiate(&required_capability()), Negotiation::Unavailable);
    }
}
