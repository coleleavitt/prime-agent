//! When a task actually fires: Claude Code's deterministic per-task jitter
//! on top of the cron match, and the recurring-task age limit.
//!
//! Recurring tasks fire up to 10% of their period late (at most 15
//! minutes); one-shot tasks landing on `:00` or `:30` fire up to 90 s early.
//! The offset is a fraction keyed on the task id, so a task keeps its offset
//! across restarts and different tasks spread out.

use super::expr::{CronZone, local_minute, next_fire};

/// Claude Code's `tengu_kairos_cron_config` defaults, as its `CronCreate`
/// documentation states them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JitterConfig {
    /// The share of a recurring period a fire may land late.
    pub recurring_frac: f64,
    /// The cap on a recurring task's lateness.
    pub recurring_cap_ms: i64,
    /// The most a one-shot fires early.
    pub one_shot_max_ms: i64,
    pub one_shot_floor_ms: i64,
    /// One-shots whose minute is a multiple of this get the early offset.
    pub one_shot_minute_mod: u32,
    /// Recurring tasks older than this fire once more, then are deleted.
    pub recurring_max_age_ms: i64,
    /// A five-minute `*/5`-style task fires this much early, so its wakeup
    /// lands inside the five-minute prompt-cache window.
    pub cache_lead_ms: i64,
}

/// The prompt cache's short time-to-live.
pub const CACHE_TTL_MS: i64 = 300_000;

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            recurring_frac: 0.1,
            recurring_cap_ms: 15 * 60_000,
            one_shot_max_ms: 90_000,
            one_shot_floor_ms: 0,
            one_shot_minute_mod: 30,
            recurring_max_age_ms: 7 * 24 * 60 * 60_000,
            cache_lead_ms: 15_000,
        }
    }
}

impl JitterConfig {
    /// The age limit in whole days (the model and the user are told it).
    #[must_use]
    pub fn max_age_days(&self) -> i64 {
        self.recurring_max_age_ms / (24 * 60 * 60_000)
    }
}

/// The task id's jitter fraction in `[0, 1)`: its first eight hex digits.
#[must_use]
pub fn jitter_fraction(id: &str) -> f64 {
    id.get(..8)
        .and_then(|prefix| u32::from_str_radix(prefix, 16).ok())
        .map_or(0.0, |value| f64::from(value) / 4_294_967_296.0)
}

/// Whether the expression is `*/N * * * *`.
fn is_minute_step(expression: &str) -> bool {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    matches!(fields.as_slice(), [minute, "*", "*", "*", "*"]
        if minute.strip_prefix("*/").is_some_and(|step| !step.is_empty() && step.bytes().all(|b| b.is_ascii_digit())))
}

/// A recurring task's next fire after `from_ms` (its last fire, else its
/// creation).
#[must_use]
pub fn recurring_fire_at(
    expression: &str,
    from_ms: i64,
    id: &str,
    config: &JitterConfig,
    zone: &dyn CronZone,
) -> Option<i64> {
    let first = next_fire(expression, from_ms, zone)?;
    let Some(second) = next_fire(expression, first, zone) else {
        return Some(first);
    };
    let period = second - first;
    if is_minute_step(expression)
        && config.cache_lead_ms > 0
        && config.cache_lead_ms < period
        && period >= CACHE_TTL_MS
        && period - config.cache_lead_ms < CACHE_TTL_MS
    {
        return Some(from_ms + period - config.cache_lead_ms);
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "a period in ms fits an f64 exactly and the jitter is far below i64::MAX"
    )]
    let late = ((jitter_fraction(id) * config.recurring_frac * period as f64) as i64)
        .min(config.recurring_cap_ms);
    Some(first + late)
}

/// A one-shot task's fire: its first match after creation, pulled early
/// when it lands on a round minute, never before `from_ms`.
#[must_use]
pub fn one_shot_fire_at(
    expression: &str,
    from_ms: i64,
    id: &str,
    config: &JitterConfig,
    zone: &dyn CronZone,
) -> Option<i64> {
    let first = next_fire(expression, from_ms, zone)?;
    if !local_minute(first, zone).is_multiple_of(config.one_shot_minute_mod.max(1)) {
        return Some(first);
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "the early window is at most minutes of ms"
    )]
    let early = config.one_shot_floor_ms
        + (jitter_fraction(id) * (config.one_shot_max_ms - config.one_shot_floor_ms) as f64) as i64;
    Some((first - early).max(from_ms))
}

/// Whether a recurring task reached the age limit at `now_ms`.
#[must_use]
pub fn aged_out(created_at_ms: i64, now_ms: i64, config: &JitterConfig) -> bool {
    config.recurring_max_age_ms > 0 && now_ms - created_at_ms >= config.recurring_max_age_ms
}
