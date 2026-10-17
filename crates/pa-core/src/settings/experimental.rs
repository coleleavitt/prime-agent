//! `experimental`: Claude Code's unreleased (flag-gated) behaviors, each OFF
//! unless its setting or environment variable turns it on.
//!
//! The settings document carries them in one `experimental` object (read
//! from the merged document's unknown keys, so the typed [`Settings`] stays
//! untouched):
//!
//! ```json
//! "experimental": {
//!   "usageLimitWrapUp": true, "usageLimitWrapUpThreshold": 0.95,
//!   "goalEvaluator": true, "goalEvaluatorModel": "provider/model-id",
//!   "modelProposedGoals": true,
//!   "idleCompaction": true, "idleCompactionMinutes": 54, "idleCompactionMinTokens": 200000,
//!   "idleCompactionAfterMs": 3240000,
//!   "observer": true, "observerModel": "provider/model-id", "observerInstructions": "...",
//!   "advisorModel": "provider/model-id"
//! }
//! ```
//!
//! An environment variable wins over the document (`1`/`true`/`on` enables,
//! `0`/`false`/`off` disables): see the `*_ENV` constants. Values of the
//! wrong type read as unset.
//!
//! [`Settings`]: super::types::Settings

use serde_json::Value;

/// The settings key holding the object.
pub const EXPERIMENTAL_KEY: &str = "experimental";

/// Disables the default-on output-limit recovery when `lengthContinuations`
/// is unset (`0`); it is pure robustness, so it is the one default-on entry.
pub const OUTPUT_LIMIT_RECOVERY_ENV: &str = "PRIME_AGENT_OUTPUT_LIMIT_RECOVERY";
/// Turns the usage-limit wrap-up on or off.
pub const USAGE_LIMIT_WRAP_UP_ENV: &str = "PRIME_AGENT_USAGE_LIMIT_WRAP_UP";
/// Turns the goal evaluator on or off.
pub const GOAL_EVALUATOR_ENV: &str = "PRIME_AGENT_GOAL_EVALUATOR";
/// Turns model-proposed goals (`rlm.propose_goal`) on or off.
pub const MODEL_PROPOSED_GOALS_ENV: &str = "PRIME_AGENT_MODEL_PROPOSED_GOALS";
/// Turns idle compaction on or off.
pub const IDLE_COMPACTION_ENV: &str = "PRIME_AGENT_IDLE_COMPACT";
/// Overrides the idle-compaction token floor (Claude Code's
/// `CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS`).
pub const IDLE_COMPACTION_MIN_TOKENS_ENV: &str = "PRIME_AGENT_IDLE_COMPACT_MIN_TOKENS";
/// Turns observer agents on or off (Claude Code's
/// `CLAUDE_CODE_EXPERIMENTAL_OBSERVER_AGENTS`).
pub const OBSERVER_ENV: &str = "PRIME_AGENT_EXPERIMENTAL_OBSERVER_AGENTS";
/// Names the advisor model (`provider/model-id`), enabling `rlm.advisor`.
pub const ADVISOR_MODEL_ENV: &str = "PRIME_AGENT_ADVISOR_MODEL";

/// Claude Code's near-limit threshold on the five-hour window's utilization
/// (`cS`).
pub const DEFAULT_USAGE_LIMIT_WRAP_UP_THRESHOLD: f64 = 0.95;
/// Claude Code fires idle compaction at 0.9 of the one-hour prompt-cache
/// TTL (`fireAtFraction` default `Cao`): 54 minutes.
pub const DEFAULT_IDLE_COMPACTION_MINUTES: u64 = 54;
/// Claude Code's default idle-compaction token floor (`_ao`).
pub const DEFAULT_IDLE_COMPACTION_MIN_TOKENS: u64 = 200_000;
/// Claude Code's lowest accepted idle-compaction floor (`Rao`).
pub const MIN_IDLE_COMPACTION_MIN_TOKENS: u64 = 100_000;

/// The resolved `experimental` knobs.
#[derive(Debug, Clone, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent on/off switches, one per flag-gated behavior"
)]
pub struct ExperimentalSettings {
    pub usage_limit_wrap_up: bool,
    /// Utilization fraction (0..=1) at which the wrap-up note arms.
    pub usage_limit_wrap_up_threshold: f64,
    pub goal_evaluator: bool,
    /// `provider/model-id` for the evaluator; `None` uses the session model.
    pub goal_evaluator_model: Option<String>,
    pub model_proposed_goals: bool,
    pub idle_compaction: bool,
    /// How long a session sits idle before it compacts:
    /// `idleCompactionAfterMs`, else `idleCompactionMinutes` (Claude Code's
    /// `CLAUDE_CODE_SLEEP_COMPACT_AFTER_MS` / `fireAtFraction` of the cache
    /// TTL).
    pub idle_compaction_after_ms: u64,
    pub idle_compaction_min_tokens: u64,
    pub observer: bool,
    /// `provider/model-id` for the observer; `None` uses the session model.
    pub observer_model: Option<String>,
    /// Supplemental postamble appended to each digest (Claude Code's
    /// `observerMessage`).
    pub observer_instructions: Option<String>,
    /// `provider/model-id` of the advisor; `None` leaves `rlm.advisor` off.
    pub advisor_model: Option<String>,
}

impl Default for ExperimentalSettings {
    fn default() -> Self {
        Self {
            usage_limit_wrap_up: false,
            usage_limit_wrap_up_threshold: DEFAULT_USAGE_LIMIT_WRAP_UP_THRESHOLD,
            goal_evaluator: false,
            goal_evaluator_model: None,
            model_proposed_goals: false,
            idle_compaction: false,
            idle_compaction_after_ms: DEFAULT_IDLE_COMPACTION_MINUTES * 60_000,
            idle_compaction_min_tokens: DEFAULT_IDLE_COMPACTION_MIN_TOKENS,
            observer: false,
            observer_model: None,
            observer_instructions: None,
            advisor_model: None,
        }
    }
}

/// An environment flag's value: `Some(true)` for `1`/`true`/`on`/`yes`,
/// `Some(false)` for `0`/`false`/`off`/`no`, `None` otherwise.
#[must_use]
pub fn env_flag(value: Option<&str>) -> Option<bool> {
    match value.map(|raw| raw.trim().to_ascii_lowercase()).as_deref() {
        Some("1" | "true" | "on" | "yes") => Some(true),
        Some("0" | "false" | "off" | "no") => Some(false),
        _ => None,
    }
}

fn non_empty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

impl ExperimentalSettings {
    /// Resolve the knobs from the merged document's `experimental` object
    /// and the environment (`env` looks a variable up; production passes
    /// `std::env::var`).
    #[must_use]
    pub fn resolve(object: Option<&Value>, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let defaults = Self::default();
        let get = |key: &str| object.and_then(|object| object.get(key));
        let flag = |key: &str, env_name: &str| {
            env_flag(env(env_name).as_deref())
                .or_else(|| get(key).and_then(Value::as_bool))
                .unwrap_or(false)
        };
        let threshold = get("usageLimitWrapUpThreshold")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value > 0.0 && *value <= 1.0)
            .unwrap_or(defaults.usage_limit_wrap_up_threshold);
        let idle_after_ms = get("idleCompactionAfterMs")
            .and_then(Value::as_u64)
            .filter(|ms| *ms > 0)
            .or_else(|| {
                get("idleCompactionMinutes")
                    .and_then(Value::as_u64)
                    .filter(|minutes| *minutes > 0)
                    .map(|minutes| minutes.saturating_mul(60_000))
            })
            .unwrap_or(defaults.idle_compaction_after_ms);
        let idle_min_tokens = env(IDLE_COMPACTION_MIN_TOKENS_ENV)
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .or_else(|| get("idleCompactionMinTokens").and_then(Value::as_u64))
            .map_or(defaults.idle_compaction_min_tokens, |tokens| {
                tokens.max(MIN_IDLE_COMPACTION_MIN_TOKENS)
            });
        Self {
            usage_limit_wrap_up: flag("usageLimitWrapUp", USAGE_LIMIT_WRAP_UP_ENV),
            usage_limit_wrap_up_threshold: threshold,
            goal_evaluator: flag("goalEvaluator", GOAL_EVALUATOR_ENV),
            goal_evaluator_model: non_empty(get("goalEvaluatorModel")),
            model_proposed_goals: flag("modelProposedGoals", MODEL_PROPOSED_GOALS_ENV),
            idle_compaction: flag("idleCompaction", IDLE_COMPACTION_ENV),
            idle_compaction_after_ms: idle_after_ms,
            idle_compaction_min_tokens: idle_min_tokens,
            observer: flag("observer", OBSERVER_ENV),
            observer_model: non_empty(get("observerModel")),
            observer_instructions: non_empty(get("observerInstructions")),
            advisor_model: env(ADVISOR_MODEL_ENV)
                .map(|raw| raw.trim().to_string())
                .filter(|raw| !raw.is_empty())
                .or_else(|| non_empty(get("advisorModel"))),
        }
    }
}

/// How a reply cut off at the output-token limit is recovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLimitRecovery {
    /// A truncated reply ends the turn.
    Off,
    /// `lengthContinuations: n` (upstream #969): up to `n` visible
    /// `[auto-continue i/n ...]` rows.
    Continuations(u32),
    /// Claude Code's recovery (the default when `lengthContinuations` is
    /// unset): up to [`CLAUDE_CODE_OUTPUT_LIMIT_RECOVERIES`] hidden
    /// "Output token limit hit. Resume directly ..." nudges, reasoning-only
    /// cuts included.
    ClaudeCode,
}

/// Claude Code's bound on consecutive output-limit recoveries (`Cr`).
pub const CLAUDE_CODE_OUTPUT_LIMIT_RECOVERIES: u32 = 3;

/// Claude Code's output-limit nudge (`Mct`), verbatim.
pub const OUTPUT_LIMIT_RECOVERY_TEXT: &str = "Output token limit hit. Resume directly \u{2014} no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.";

impl OutputLimitRecovery {
    /// The recovery for an explicit `lengthContinuations` value (`None` when
    /// unset) and the [`OUTPUT_LIMIT_RECOVERY_ENV`] value.
    #[must_use]
    pub fn resolve(length_continuations: Option<u32>, env: Option<&str>) -> Self {
        match length_continuations {
            Some(0) => OutputLimitRecovery::Off,
            Some(count) => OutputLimitRecovery::Continuations(count),
            None => match env_flag(env) {
                Some(false) => OutputLimitRecovery::Off,
                Some(true) | None => OutputLimitRecovery::ClaudeCode,
            },
        }
    }
}

impl super::manager::SettingsManager {
    /// The `experimental` knobs, with the process environment's overrides.
    #[must_use]
    pub fn experimental(&self) -> ExperimentalSettings {
        ExperimentalSettings::resolve(self.settings().extra.get(EXPERIMENTAL_KEY), &|name| {
            std::env::var(name).ok()
        })
    }

    /// How a reply cut off at the output-token limit is recovered: an
    /// explicit `lengthContinuations` decides (clamped like
    /// [`Self::get_length_continuations`]); unset is Claude Code's recovery
    /// unless [`OUTPUT_LIMIT_RECOVERY_ENV`] turns it off.
    #[must_use]
    pub fn get_output_limit_recovery(&self) -> OutputLimitRecovery {
        let explicit = self
            .settings()
            .length_continuations
            .map(|_| self.get_length_continuations());
        OutputLimitRecovery::resolve(
            explicit,
            std::env::var(OUTPUT_LIMIT_RECOVERY_ENV).ok().as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn everything_is_off_without_the_object_or_the_environment() {
        assert_eq!(
            ExperimentalSettings::resolve(None, &env_of(&[])),
            ExperimentalSettings::default()
        );
    }

    #[test]
    fn the_object_turns_knobs_on_and_bounds_their_values() {
        let object = serde_json::json!({
            "usageLimitWrapUp": true,
            "usageLimitWrapUpThreshold": 0.8,
            "goalEvaluator": true,
            "goalEvaluatorModel": " anthropic/claude-haiku ",
            "modelProposedGoals": true,
            "idleCompaction": true,
            "idleCompactionMinutes": 4,
            "idleCompactionMinTokens": 5,
            "observer": true,
            "observerModel": "",
            "observerInstructions": "watch the tests",
            "advisorModel": "anthropic/claude-opus",
        });
        assert_eq!(
            ExperimentalSettings::resolve(Some(&object), &env_of(&[])),
            ExperimentalSettings {
                usage_limit_wrap_up: true,
                usage_limit_wrap_up_threshold: 0.8,
                goal_evaluator: true,
                goal_evaluator_model: Some("anthropic/claude-haiku".to_string()),
                model_proposed_goals: true,
                idle_compaction: true,
                idle_compaction_after_ms: 240_000,
                idle_compaction_min_tokens: MIN_IDLE_COMPACTION_MIN_TOKENS,
                observer: true,
                observer_model: None,
                observer_instructions: Some("watch the tests".to_string()),
                advisor_model: Some("anthropic/claude-opus".to_string()),
            }
        );
    }

    #[test]
    fn the_environment_wins_over_the_object() {
        let object = serde_json::json!({ "observer": true, "goalEvaluator": false, "usageLimitWrapUpThreshold": 7 });
        let resolved = ExperimentalSettings::resolve(
            Some(&object),
            &env_of(&[
                (OBSERVER_ENV, "0"),
                (GOAL_EVALUATOR_ENV, "on"),
                (IDLE_COMPACTION_MIN_TOKENS_ENV, "300000"),
                (ADVISOR_MODEL_ENV, "openai/gpt-x"),
            ]),
        );
        assert_eq!(
            resolved,
            ExperimentalSettings {
                goal_evaluator: true,
                idle_compaction_min_tokens: 300_000,
                advisor_model: Some("openai/gpt-x".to_string()),
                ..ExperimentalSettings::default()
            }
        );
    }

    #[test]
    fn output_limit_recovery_defaults_to_claude_codes_and_yields_to_the_setting() {
        assert_eq!(
            OutputLimitRecovery::resolve(None, None),
            OutputLimitRecovery::ClaudeCode
        );
        assert_eq!(
            OutputLimitRecovery::resolve(None, Some("0")),
            OutputLimitRecovery::Off
        );
        assert_eq!(
            OutputLimitRecovery::resolve(Some(0), Some("1")),
            OutputLimitRecovery::Off
        );
        assert_eq!(
            OutputLimitRecovery::resolve(Some(2), Some("0")),
            OutputLimitRecovery::Continuations(2)
        );
    }
}
