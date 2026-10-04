//! The typed, serializable exploration policy and its soundness-critical
//! parser (TS `policy.ts`).
//!
//! A policy is DATA, never code: ten fields of bounded numbers and named-rule
//! literals. The only thing that acts on one is the fixed interpreter
//! (`interpreter.rs`). [`parse_exploration_policy`] rejects any unknown key,
//! any out-of-range or wrong-typed number and any rule outside the literal
//! sets, so a smuggled code payload can only arrive as an extra key or a wrong
//! type, and both are rejected.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::json;

macro_rules! named_rules {
    ($(#[$meta:meta])* $name:ident, $all:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            /// The wire literal.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// The rule named by `text`, if it is one of the literals.
            #[must_use]
            pub fn from_name(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        /// Every literal, in schema order.
        pub const $all: &[$name] = &[$($name::$variant),+];
    };
}

named_rules!(
    /// How eligible cells are ranked before the batch is cut.
    SelectionRule, SELECTION_RULES {
        BestFirst => "best-first",
        ExploreRoot => "explore-root",
        RoundRobin => "round-robin",
        Weighted => "weighted",
    }
);

named_rules!(
    /// What a failed branch falls back to (advisory; read nowhere).
    RecoveryPolicy, RECOVERY_POLICIES {
        RetryBest => "retry-best",
        RetryRoot => "retry-root",
        Widen => "widen",
        Abandon => "abandon",
    }
);

named_rules!(
    /// When exploration stops.
    StopRule, STOP_RULES {
        Patience => "patience",
        Threshold => "threshold",
        FixedRounds => "fixed-rounds",
        Never => "never",
    }
);

/// The exploration policy. Integer fields are `u32`; the rest are doubles.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplorationPolicy {
    pub selection_rule: SelectionRule,
    pub recovery_policy: RecoveryPolicy,
    pub stop_rule: StopRule,
    /// Branch/step width the proposer scale derives from.
    pub branch_width: u32,
    /// Refinement candidates per generation attempt.
    pub refine_depth: u32,
    /// Cells probed per round, capped at runtime by W.
    pub batch_size: u32,
    /// Patience for `patience`, round cap for `fixed-rounds`.
    pub beta: u32,
    /// A node is promising when its score is at least this fraction of the best.
    pub promising_threshold: f64,
    /// Target best score for the `threshold` stop rule.
    pub target_score: f64,
    /// Weight the `weighted` rule puts on promising branches.
    pub exploration_bias: f64,
}

/// One policy field, in schema order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyField {
    SelectionRule,
    RecoveryPolicy,
    StopRule,
    BranchWidth,
    RefineDepth,
    BatchSize,
    Beta,
    PromisingThreshold,
    TargetScore,
    ExplorationBias,
}

impl PolicyField {
    /// The wire key.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelectionRule => "selectionRule",
            Self::RecoveryPolicy => "recoveryPolicy",
            Self::StopRule => "stopRule",
            Self::BranchWidth => "branchWidth",
            Self::RefineDepth => "refineDepth",
            Self::BatchSize => "batchSize",
            Self::Beta => "beta",
            Self::PromisingThreshold => "promisingThreshold",
            Self::TargetScore => "targetScore",
            Self::ExplorationBias => "explorationBias",
        }
    }
}

/// Every field in schema order.
pub const POLICY_FIELD_ORDER: &[PolicyField] = &[
    PolicyField::SelectionRule,
    PolicyField::RecoveryPolicy,
    PolicyField::StopRule,
    PolicyField::BranchWidth,
    PolicyField::RefineDepth,
    PolicyField::BatchSize,
    PolicyField::Beta,
    PolicyField::PromisingThreshold,
    PolicyField::TargetScore,
    PolicyField::ExplorationBias,
];

/// Fields the replay simulator never reads: `branchWidth` and `refineDepth`
/// shape every ONLINE proposal and `recoveryPolicy` is read nowhere, so a
/// candidate that differs only in them replays identically.
pub const REPLAY_DEAD_FIELDS: &[PolicyField] = &[
    PolicyField::BranchWidth,
    PolicyField::RefineDepth,
    PolicyField::RecoveryPolicy,
];

/// A numeric field's inclusive bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NumericBound {
    pub min: f64,
    pub max: f64,
    pub integer: bool,
}

/// The numeric fields with their bounds, in TS `POLICY_BOUNDS` order.
pub const POLICY_BOUNDS: &[(PolicyField, NumericBound)] = &[
    (
        PolicyField::BranchWidth,
        NumericBound {
            min: 1.0,
            max: 8.0,
            integer: true,
        },
    ),
    (
        PolicyField::RefineDepth,
        NumericBound {
            min: 0.0,
            max: 8.0,
            integer: true,
        },
    ),
    (
        PolicyField::BatchSize,
        NumericBound {
            min: 1.0,
            max: 8.0,
            integer: true,
        },
    ),
    (
        PolicyField::Beta,
        NumericBound {
            min: 1.0,
            max: 32.0,
            integer: true,
        },
    ),
    (
        PolicyField::PromisingThreshold,
        NumericBound {
            min: 0.0,
            max: 1.0,
            integer: false,
        },
    ),
    (
        PolicyField::TargetScore,
        NumericBound {
            min: 0.0,
            max: 1_000_000.0,
            integer: false,
        },
    ),
    (
        PolicyField::ExplorationBias,
        NumericBound {
            min: 0.0,
            max: 1.0,
            integer: false,
        },
    ),
];

/// The bound of one numeric field.
#[must_use]
pub fn bound_of(field: PolicyField) -> Option<NumericBound> {
    POLICY_BOUNDS
        .iter()
        .find(|(known, _)| *known == field)
        .map(|(_, bound)| *bound)
}

/// A parallel-refine default: refine the best nodes in parallel, stop on patience.
pub const DEFAULT_POLICY: ExplorationPolicy = ExplorationPolicy {
    selection_rule: SelectionRule::BestFirst,
    recovery_policy: RecoveryPolicy::RetryBest,
    stop_rule: StopRule::Patience,
    branch_width: 2,
    refine_depth: 2,
    batch_size: 4,
    beta: 6,
    promising_threshold: 0.5,
    target_score: 1_000_000.0,
    exploration_bias: 0.25,
};

/// The fixed `--priming diverse` set: breadth at the root, and one greedy chain.
pub const PRIMING_DIVERSE: [ExplorationPolicy; 2] = [
    ExplorationPolicy {
        selection_rule: SelectionRule::ExploreRoot,
        stop_rule: StopRule::Never,
        batch_size: 8,
        ..DEFAULT_POLICY
    },
    ExplorationPolicy {
        selection_rule: SelectionRule::BestFirst,
        stop_rule: StopRule::Never,
        batch_size: 1,
        ..DEFAULT_POLICY
    },
];

impl ExplorationPolicy {
    /// A numeric field's value as a double.
    #[must_use]
    pub fn numeric(&self, field: PolicyField) -> Option<f64> {
        match field {
            PolicyField::BranchWidth => Some(f64::from(self.branch_width)),
            PolicyField::RefineDepth => Some(f64::from(self.refine_depth)),
            PolicyField::BatchSize => Some(f64::from(self.batch_size)),
            PolicyField::Beta => Some(f64::from(self.beta)),
            PolicyField::PromisingThreshold => Some(self.promising_threshold),
            PolicyField::TargetScore => Some(self.target_score),
            PolicyField::ExplorationBias => Some(self.exploration_bias),
            PolicyField::SelectionRule | PolicyField::RecoveryPolicy | PolicyField::StopRule => {
                None
            }
        }
    }

    fn field_equals(&self, other: &Self, field: PolicyField) -> bool {
        match field {
            PolicyField::SelectionRule => self.selection_rule == other.selection_rule,
            PolicyField::RecoveryPolicy => self.recovery_policy == other.recovery_policy,
            PolicyField::StopRule => self.stop_rule == other.stop_rule,
            numeric => self.numeric(numeric) == other.numeric(numeric),
        }
    }

    /// The policy as a JSON object, keys in schema order.
    #[must_use]
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// The fields on which two policies differ, in schema order.
#[must_use]
pub fn policy_fields_differing(a: &ExplorationPolicy, b: &ExplorationPolicy) -> Vec<PolicyField> {
    POLICY_FIELD_ORDER
        .iter()
        .copied()
        .filter(|field| !a.field_equals(b, *field))
        .collect()
}

/// True when `candidate` differs from `current` and every differing field is replay-dead.
#[must_use]
pub fn differs_only_in_replay_dead_fields(
    candidate: &ExplorationPolicy,
    current: &ExplorationPolicy,
) -> bool {
    let changed = policy_fields_differing(candidate, current);
    !changed.is_empty()
        && changed
            .iter()
            .all(|field| REPLAY_DEAD_FIELDS.contains(field))
}

/// Why a JSON value is not a policy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PolicyValidationError(pub String);

/// Strict schema parse: every known key present and in range, no unknown key.
///
/// # Errors
///
/// [`PolicyValidationError`] naming the first offending field.
pub fn parse_exploration_policy(value: &Value) -> Result<ExplorationPolicy, PolicyValidationError> {
    let Value::Object(map) = value else {
        return Err(PolicyValidationError(
            "policy must be a JSON object".to_string(),
        ));
    };
    for key in map.keys() {
        if !POLICY_FIELD_ORDER.iter().any(|field| field.as_str() == key) {
            return Err(PolicyValidationError(format!(
                "unknown policy field: {key}"
            )));
        }
    }
    let mut numbers = Vec::with_capacity(POLICY_BOUNDS.len());
    for (field, bound) in POLICY_BOUNDS {
        let name = field.as_str();
        let raw = map
            .get(name)
            .and_then(Value::as_f64)
            .filter(|raw| raw.is_finite());
        let Some(raw) = raw else {
            return Err(PolicyValidationError(format!(
                "{name} must be a finite number"
            )));
        };
        if bound.integer && raw.fract() != 0.0 {
            return Err(PolicyValidationError(format!("{name} must be an integer")));
        }
        if raw < bound.min || raw > bound.max {
            return Err(PolicyValidationError(format!(
                "{name} must be within [{}, {}]",
                json::js_number(bound.min),
                json::js_number(bound.max)
            )));
        }
        numbers.push(raw);
    }
    let rule = |field: PolicyField, names: &[&str]| -> Result<String, PolicyValidationError> {
        match map.get(field.as_str()) {
            Some(Value::String(text)) if names.contains(&text.as_str()) => Ok(text.clone()),
            _ => Err(PolicyValidationError(format!(
                "{} must be one of {}",
                field.as_str(),
                names.join(", ")
            ))),
        }
    };
    let selection: Vec<&str> = SELECTION_RULES.iter().map(|rule| rule.as_str()).collect();
    let recovery: Vec<&str> = RECOVERY_POLICIES.iter().map(|rule| rule.as_str()).collect();
    let stop: Vec<&str> = STOP_RULES.iter().map(|rule| rule.as_str()).collect();
    let selection_rule = rule(PolicyField::SelectionRule, &selection)?;
    let recovery_policy = rule(PolicyField::RecoveryPolicy, &recovery)?;
    let stop_rule = rule(PolicyField::StopRule, &stop)?;
    let integer = |value: f64| -> u32 { as_u32(value) };
    Ok(ExplorationPolicy {
        selection_rule: SelectionRule::from_name(&selection_rule)
            .unwrap_or(SelectionRule::BestFirst),
        recovery_policy: RecoveryPolicy::from_name(&recovery_policy)
            .unwrap_or(RecoveryPolicy::RetryBest),
        stop_rule: StopRule::from_name(&stop_rule).unwrap_or(StopRule::Patience),
        branch_width: integer(numbers[0]),
        refine_depth: integer(numbers[1]),
        batch_size: integer(numbers[2]),
        beta: integer(numbers[3]),
        promising_threshold: numbers[4],
        target_score: numbers[5],
        exploration_bias: numbers[6],
    })
}

/// A bounded, integral, non-negative double as `u32`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn as_u32(value: f64) -> u32 {
    value.clamp(0.0, f64::from(u32::MAX)) as u32
}

/// `Math.round`: halves round towards +infinity.
#[must_use]
pub fn js_round(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

fn clamp_number(raw: Option<&Value>, bound: NumericBound, fallback: f64) -> f64 {
    let Some(raw) = raw.and_then(Value::as_f64).filter(|raw| raw.is_finite()) else {
        return fallback;
    };
    let clamped = bound.max.min(bound.min.max(raw));
    if bound.integer {
        js_round(clamped)
    } else {
        clamped
    }
}

/// Project an arbitrary JSON object into a valid policy: out-of-range numbers
/// clamped (and rounded where an integer is required), invalid rules snapped
/// to the default, unknown keys dropped. Never fails.
#[must_use]
pub fn clamp_policy(raw: &Value) -> ExplorationPolicy {
    let empty = Map::new();
    let source = raw.as_object().unwrap_or(&empty);
    let numeric = |field: PolicyField| -> f64 {
        let bound = bound_of(field).unwrap_or(NumericBound {
            min: 0.0,
            max: 0.0,
            integer: false,
        });
        let fallback = DEFAULT_POLICY.numeric(field).unwrap_or(0.0);
        clamp_number(source.get(field.as_str()), bound, fallback)
    };
    let text = |field: PolicyField| source.get(field.as_str()).and_then(Value::as_str);
    ExplorationPolicy {
        selection_rule: text(PolicyField::SelectionRule)
            .and_then(SelectionRule::from_name)
            .unwrap_or(DEFAULT_POLICY.selection_rule),
        recovery_policy: text(PolicyField::RecoveryPolicy)
            .and_then(RecoveryPolicy::from_name)
            .unwrap_or(DEFAULT_POLICY.recovery_policy),
        stop_rule: text(PolicyField::StopRule)
            .and_then(StopRule::from_name)
            .unwrap_or(DEFAULT_POLICY.stop_rule),
        branch_width: as_u32(numeric(PolicyField::BranchWidth)),
        refine_depth: as_u32(numeric(PolicyField::RefineDepth)),
        batch_size: as_u32(numeric(PolicyField::BatchSize)),
        beta: as_u32(numeric(PolicyField::Beta)),
        promising_threshold: numeric(PolicyField::PromisingThreshold),
        target_score: numeric(PolicyField::TargetScore),
        exploration_bias: numeric(PolicyField::ExplorationBias),
    }
}

/// A stable identity: the first 16 hex digits of the sha256 of the canonical
/// JSON of exactly the policy's fields.
#[must_use]
pub fn policy_id(policy: &ExplorationPolicy) -> String {
    let digest = sha256_hex(json::canonical_json(&policy.to_value()).as_bytes());
    digest[..16].to_string()
}

/// Lowercase hex sha256.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_default_policy_id_matches_the_ts_product() {
        // `1be99d403b0405a3` is the id every recorded TS run reports for DEFAULT_POLICY.
        assert_eq!(policy_id(&DEFAULT_POLICY), "1be99d403b0405a3");
        let collapsed = ExplorationPolicy {
            selection_rule: SelectionRule::Weighted,
            stop_rule: StopRule::FixedRounds,
            beta: 1,
            ..DEFAULT_POLICY
        };
        assert_eq!(policy_id(&collapsed), "f559ec93fc3b1773");
    }

    #[test]
    fn parse_accepts_the_default_in_any_key_order_and_rejects_everything_else() {
        let reordered = json!({
            "targetScore": 1_000_000, "stopRule": "patience", "batchSize": 4, "explorationBias": 0.25,
            "selectionRule": "best-first", "beta": 6, "branchWidth": 2, "promisingThreshold": 0.5,
            "recoveryPolicy": "retry-best", "refineDepth": 2
        });
        assert_eq!(parse_exploration_policy(&reordered), Ok(DEFAULT_POLICY));
        let with = |key: &str, value: Value| {
            let mut object = DEFAULT_POLICY.to_value();
            object[key] = value;
            parse_exploration_policy(&object)
        };
        let rejected = [
            parse_exploration_policy(&Value::Null),
            parse_exploration_policy(&json!([DEFAULT_POLICY.to_value()])),
            parse_exploration_policy(&json!("policy")),
            with("extra", json!(1)),
            with("batchSize", json!(999)),
            with("promisingThreshold", json!(-0.1)),
            with("branchWidth", json!(2.5)),
            with("targetScore", Value::Null),
            with("selectionRule", json!("nope")),
            with("stopRule", json!("halt")),
            with("propose", json!("() => process.exit(1)")),
            with("batchSize", json!("() => 2")),
        ];
        assert!(rejected.iter().all(Result::is_err), "{rejected:?}");
        assert_eq!(
            with("extra", json!(1)),
            Err(PolicyValidationError(
                "unknown policy field: extra".to_string()
            ))
        );
    }

    #[test]
    fn clamp_projects_into_bounds_rounds_integers_and_snaps_rules() {
        let mut raw = DEFAULT_POLICY.to_value();
        raw["batchSize"] = json!(999);
        raw["branchWidth"] = json!(2.4);
        raw["promisingThreshold"] = json!(-5);
        raw["explorationBias"] = json!(100);
        raw["selectionRule"] = json!("nope");
        raw["stopRule"] = json!(42);
        raw["junk"] = json!("x");
        assert_eq!(
            clamp_policy(&raw),
            ExplorationPolicy {
                batch_size: 8,
                branch_width: 2,
                promising_threshold: 0.0,
                exploration_bias: 1.0,
                ..DEFAULT_POLICY
            }
        );
        assert_eq!(clamp_policy(&Value::Null), DEFAULT_POLICY);
        assert_eq!(js_round(2.5).to_bits(), 3.0f64.to_bits());
        assert_eq!(
            js_round(0.499_999_999_999_999_94).to_bits(),
            0.0f64.to_bits()
        );
    }

    #[test]
    fn differing_fields_come_in_schema_order_and_replay_dead_ones_are_flagged() {
        let changed = ExplorationPolicy {
            beta: 3,
            selection_rule: SelectionRule::Weighted,
            refine_depth: 1,
            ..DEFAULT_POLICY
        };
        assert_eq!(
            policy_fields_differing(&changed, &DEFAULT_POLICY),
            vec![
                PolicyField::SelectionRule,
                PolicyField::RefineDepth,
                PolicyField::Beta
            ]
        );
        let dead = ExplorationPolicy {
            recovery_policy: RecoveryPolicy::Widen,
            refine_depth: 0,
            ..DEFAULT_POLICY
        };
        assert!(differs_only_in_replay_dead_fields(&dead, &DEFAULT_POLICY));
        assert!(!differs_only_in_replay_dead_fields(
            &DEFAULT_POLICY,
            &DEFAULT_POLICY
        ));
        assert!(!differs_only_in_replay_dead_fields(
            &ExplorationPolicy {
                branch_width: 5,
                beta: 2,
                ..DEFAULT_POLICY
            },
            &DEFAULT_POLICY
        ));
        for policy in &PRIMING_DIVERSE {
            assert_eq!(parse_exploration_policy(&policy.to_value()), Ok(*policy));
            assert!(!differs_only_in_replay_dead_fields(policy, &DEFAULT_POLICY));
        }
    }
}
