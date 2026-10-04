//! The cohort comparison over the learning index (TS `buildLearningReport`):
//! fingerprints named in a `refinement.committed` are the treated cohort,
//! every other observed failure fingerprint the control, and the statistic
//! is a one-sided Mann-Whitney U on the per-fingerprint change in failure
//! rate. Nothing is randomized, so this measures association; the p-value is
//! withheld when either cohort is smaller than the minimum.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::{Map, Value};

use crate::index::{LearningDay, TURN_SPAN_NAME};
use crate::json::number;

/// Cohort size below which the p-value is withheld.
pub const DEFAULT_MIN_COHORT_N: u64 = 5;
/// Rates are per this many turns.
pub const RATE_DENOMINATOR: f64 = 1000.0;

/// One fingerprint's change across the pivot.
#[derive(Debug, Clone, PartialEq)]
pub struct FingerprintTrend {
    pub fingerprint: String,
    pub name: String,
    pub message: String,
    pub treated: bool,
    pub before_count: u64,
    pub after_count: u64,
    /// Failures per [`RATE_DENOMINATOR`] turns in the window.
    pub before_rate: f64,
    pub after_rate: f64,
    /// `after_rate - before_rate`; negative means the failure got rarer.
    pub delta: f64,
}

/// One cohort's members and deltas.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CohortStats {
    pub ids: Vec<String>,
    pub median_delta: f64,
    pub mean_delta: f64,
    pub deltas: Vec<f64>,
}

impl CohortStats {
    #[must_use]
    pub fn n(&self) -> usize {
        self.ids.len()
    }
}

/// Which side of the pivot a day is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Before,
    Pivot,
    After,
}

impl Window {
    fn as_str(self) -> &'static str {
        match self {
            Window::Before => "before",
            Window::Pivot => "pivot",
            Window::After => "after",
        }
    }
}

/// One day of the chart series.
#[derive(Debug, Clone, PartialEq)]
pub struct LearningDayPoint {
    pub day: String,
    pub turns: u64,
    pub window: Window,
    /// Mean per-fingerprint failure rate across the cohort.
    pub treated_rate: f64,
    pub untreated_rate: f64,
}

/// The report (TS `LearningReport`).
#[derive(Debug, Clone, PartialEq)]
pub struct LearningReport {
    pub generated_at: String,
    pub days: Vec<String>,
    /// The commit splitting before from after: the median by time.
    pub pivot_at: Option<String>,
    pub pivot_day: Option<String>,
    pub commits: usize,
    pub turns_before: u64,
    pub turns_after: u64,
    pub fingerprints: Vec<FingerprintTrend>,
    /// Fingerprints named in a commit and never observed.
    pub unobserved_treated: Vec<String>,
    pub treated: CohortStats,
    pub untreated: CohortStats,
    pub series: Vec<LearningDayPoint>,
    pub u: Option<f64>,
    pub p_value: Option<f64>,
    /// Why the p-value was withheld; exclusive with `p_value`.
    pub insufficient_evidence: Option<String>,
    pub min_cohort_n: u64,
}

impl LearningReport {
    /// The report as the TS object (key order included); `extra` keys (the
    /// command's `indexDir`, `seal`) follow it, as `{...report, ...}` puts
    /// them.
    #[must_use]
    pub fn to_json(&self, extra: Vec<(&str, Value)>) -> Value {
        let mut out = Map::new();
        out.insert(
            "schema".into(),
            Value::from(crate::index::LEARNING_INDEX_SCHEMA),
        );
        out.insert(
            "generatedAt".into(),
            Value::from(self.generated_at.as_str()),
        );
        out.insert("days".into(), Value::from(self.days.clone()));
        if let Some(pivot_at) = &self.pivot_at {
            out.insert("pivotAt".into(), Value::from(pivot_at.as_str()));
        }
        if let Some(pivot_day) = &self.pivot_day {
            out.insert("pivotDay".into(), Value::from(pivot_day.as_str()));
        }
        out.insert("commits".into(), Value::from(self.commits));
        let mut turns = Map::new();
        turns.insert("before".into(), Value::from(self.turns_before));
        turns.insert("after".into(), Value::from(self.turns_after));
        out.insert("turns".into(), Value::Object(turns));
        out.insert(
            "fingerprints".into(),
            Value::Array(self.fingerprints.iter().map(trend_json).collect()),
        );
        out.insert(
            "unobservedTreated".into(),
            Value::from(self.unobserved_treated.clone()),
        );
        let mut cohorts = Map::new();
        cohorts.insert("treated".into(), cohort_json(&self.treated));
        cohorts.insert("untreated".into(), cohort_json(&self.untreated));
        out.insert("cohorts".into(), Value::Object(cohorts));
        out.insert(
            "series".into(),
            Value::Array(self.series.iter().map(point_json).collect()),
        );
        if let Some(u) = self.u {
            out.insert("u".into(), number(u));
        }
        if let Some(p_value) = self.p_value {
            out.insert("pValue".into(), number(p_value));
        }
        if let Some(reason) = &self.insufficient_evidence {
            out.insert("insufficientEvidence".into(), Value::from(reason.as_str()));
        }
        out.insert("minCohortN".into(), Value::from(self.min_cohort_n));
        out.insert("rateDenominator".into(), number(RATE_DENOMINATOR));
        for (key, value) in extra {
            out.insert(key.to_string(), value);
        }
        Value::Object(out)
    }
}

fn trend_json(trend: &FingerprintTrend) -> Value {
    let mut out = Map::new();
    out.insert(
        "fingerprint".into(),
        Value::from(trend.fingerprint.as_str()),
    );
    out.insert("name".into(), Value::from(trend.name.as_str()));
    out.insert("message".into(), Value::from(trend.message.as_str()));
    out.insert("treated".into(), Value::from(trend.treated));
    out.insert("beforeCount".into(), Value::from(trend.before_count));
    out.insert("afterCount".into(), Value::from(trend.after_count));
    out.insert("beforeRate".into(), number(trend.before_rate));
    out.insert("afterRate".into(), number(trend.after_rate));
    out.insert("delta".into(), number(trend.delta));
    Value::Object(out)
}

fn cohort_json(cohort: &CohortStats) -> Value {
    let mut out = Map::new();
    out.insert("ids".into(), Value::from(cohort.ids.clone()));
    out.insert("n".into(), Value::from(cohort.n()));
    out.insert("medianDelta".into(), number(cohort.median_delta));
    out.insert("meanDelta".into(), number(cohort.mean_delta));
    out.insert(
        "deltas".into(),
        Value::Array(cohort.deltas.iter().copied().map(number).collect()),
    );
    Value::Object(out)
}

fn point_json(point: &LearningDayPoint) -> Value {
    let mut out = Map::new();
    out.insert("day".into(), Value::from(point.day.as_str()));
    out.insert("turns".into(), Value::from(point.turns));
    out.insert("window".into(), Value::from(point.window.as_str()));
    out.insert("treatedRate".into(), number(point.treated_rate));
    out.insert("untreatedRate".into(), number(point.untreated_rate));
    Value::Object(out)
}

/// Abramowitz & Stegun 7.1.26 (absolute error below 1.5e-7).
fn erf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * z);
    let poly = ((((1.061_405_429 * t - 1.453_152_027) * t + 1.421_413_741) * t - 0.284_496_736)
        * t
        + 0.254_829_592)
        * t;
    sign * (1.0 - poly * fdlibm_exp(-z * z))
}

/// `Math.exp` as V8 computes it (fdlibm `__ieee754_exp`, which V8's
/// `base::ieee754::exp` is): the platform `exp` may round its last bit
/// differently, and a p-value must not depend on the libm.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "fdlibm's bit-level word arithmetic"
)]
// fdlibm's names (`x`, `k`, `t`, `c`, `y`), kept so the port reads against it.
#[allow(clippy::many_single_char_names)]
fn fdlibm_exp(x: f64) -> f64 {
    // fdlibm's constants, by their bits (`e_exp.c`).
    const HALF: [f64; 2] = [0.5, -0.5];
    const HUGE: f64 = 1.0e+300;
    let twom1000 = f64::from_bits(0x0170_0000_0000_0000);
    let o_threshold = f64::from_bits(0x4086_2e42_fefa_39ef);
    let u_threshold = f64::from_bits(0xc087_4910_d52d_3051);
    let ln2_hi = f64::from_bits(0x3fe6_2e42_fee0_0000);
    let ln2_lo = f64::from_bits(0x3dea_39ef_3579_3c76);
    let ln2_hi = [ln2_hi, -ln2_hi];
    let ln2_lo = [ln2_lo, -ln2_lo];
    let invln2 = f64::from_bits(0x3ff7_1547_652b_82fe);
    let p1 = f64::from_bits(0x3fc5_5555_5555_553e);
    let p2 = f64::from_bits(0xbf66_c16c_16be_bd93);
    let p3 = f64::from_bits(0x3f11_566a_af25_de2c);
    let p4 = f64::from_bits(0xbebb_bd41_c5d2_6bf1);
    let p5 = f64::from_bits(0x3e66_3769_72be_a4d0);

    let bits = x.to_bits();
    let high = (bits >> 32) as u32;
    let sign = ((high >> 31) & 1) as usize;
    let high = high & 0x7fff_ffff;
    if high >= 0x4086_2e42 {
        if high >= 0x7ff0_0000 {
            if ((high & 0xf_ffff) | (bits as u32)) != 0 {
                return x + x;
            }
            return if sign == 0 { x } else { 0.0 };
        }
        if x > o_threshold {
            return HUGE * HUGE;
        }
        if x < u_threshold {
            return twom1000 * twom1000;
        }
    }
    let mut x = x;
    let (mut hi, mut lo, mut k) = (0.0, 0.0, 0i32);
    if high > 0x3fd6_2e42 {
        if high < 0x3ff0_a2b2 {
            hi = x - ln2_hi[sign];
            lo = ln2_lo[sign];
            k = 1 - 2 * sign as i32;
        } else {
            k = (invln2 * x + HALF[sign]) as i32;
            let t = f64::from(k);
            hi = x - t * ln2_hi[0];
            lo = t * ln2_lo[0];
        }
        x = hi - lo;
    } else if high < 0x3e30_0000 {
        return 1.0 + x;
    }
    let t = x * x;
    let c = x - t * (p1 + t * (p2 + t * (p3 + t * (p4 + t * p5))));
    if k == 0 {
        return 1.0 - ((x * c) / (c - 2.0) - x);
    }
    let y = 1.0 - ((lo - (x * c) / (2.0 - c)) - hi);
    let add_exponent = |y: f64, k: i32| {
        let bits = y.to_bits();
        let high = ((bits >> 32) as u32).wrapping_add((k << 20) as u32);
        f64::from_bits((u64::from(high) << 32) | (bits & 0xffff_ffff))
    };
    if k >= -1021 {
        add_exponent(y, k)
    } else {
        add_exponent(y, k + 1000) * twom1000
    }
}

/// The standard normal CDF.
#[must_use]
pub fn normal_cdf(z: f64) -> f64 {
    f64::midpoint(1.0, erf(z / std::f64::consts::SQRT_2)).clamp(0.0, 1.0)
}

/// A one-sided Mann-Whitney U result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MannWhitney {
    pub u: f64,
    pub z: f64,
    pub p_value: f64,
}

/// One-sided Mann-Whitney U with midranks and the tie correction, testing
/// "values in `lower` are smaller than values in `higher`", by the normal
/// approximation with a continuity correction (the deltas are heavily
/// tied, and an exact enumeration is not valid under ties). `None` when a
/// sample is empty or every value is the same.
#[must_use]
pub fn mann_whitney_one_sided(lower: &[f64], higher: &[f64]) -> Option<MannWhitney> {
    if lower.is_empty() || higher.is_empty() {
        return None;
    }
    #[expect(clippy::cast_precision_loss, reason = "cohort sizes")]
    let (n1, n2) = (lower.len() as f64, higher.len() as f64);
    let total = n1 + n2;
    let mut combined: Vec<(f64, bool)> = lower
        .iter()
        .map(|value| (*value, true))
        .chain(higher.iter().map(|value| (*value, false)))
        .collect();
    combined.sort_by(|left, right| left.0.total_cmp(&right.0));
    let mut rank_sum_first = 0.0;
    let mut tie_sum = 0.0;
    let mut index = 0;
    while index < combined.len() {
        let mut end = index;
        // Ties are exact equality: a tie group is equal deltas.
        #[allow(clippy::float_cmp)]
        while end + 1 < combined.len() && combined[end + 1].0 == combined[index].0 {
            end += 1;
        }
        #[expect(clippy::cast_precision_loss, reason = "cohort sizes")]
        let (size, mid_rank) = (
            (end - index + 1) as f64,
            ((index + 1) + (end + 1)) as f64 / 2.0,
        );
        for (_, first) in &combined[index..=end] {
            if *first {
                rank_sum_first += mid_rank;
            }
        }
        tie_sum += size.powi(3) - size;
        index = end + 1;
    }
    let u = rank_sum_first - (n1 * (n1 + 1.0)) / 2.0;
    let mean = (n1 * n2) / 2.0;
    let variance = ((n1 * n2) / 12.0) * (total + 1.0 - tie_sum / (total * (total - 1.0)));
    if variance.is_nan() || variance <= 0.0 {
        return None;
    }
    let z = (u - mean + 0.5) / variance.sqrt();
    Some(MannWhitney {
        u,
        z,
        p_value: normal_cdf(z),
    })
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        f64::midpoint(sorted[middle - 1], sorted[middle])
    }
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    #[expect(clippy::cast_precision_loss, reason = "cohort sizes")]
    let len = values.len() as f64;
    values.iter().fold(0.0, |sum, value| sum + value) / len
}

fn cohort(trends: &[&FingerprintTrend]) -> CohortStats {
    let deltas: Vec<f64> = trends.iter().map(|trend| trend.delta).collect();
    CohortStats {
        ids: trends
            .iter()
            .map(|trend| trend.fingerprint.clone())
            .collect(),
        median_delta: median(&deltas),
        mean_delta: mean(&deltas),
        deltas,
    }
}

#[expect(clippy::cast_precision_loss, reason = "failure and turn counts")]
fn rate(count: u64, turns: u64) -> f64 {
    if turns == 0 {
        0.0
    } else {
        (count as f64 * RATE_DENOMINATOR) / turns as f64
    }
}

/// Compare the change in per-fingerprint failure rate between the
/// fingerprints a refinement claimed to address and every other observed
/// failure fingerprint (TS `buildLearningReport`). The pivot is the median
/// `refinement.committed` by time; the pivot day itself is excluded from
/// both windows, since counting it on either side mixes exposures.
#[must_use]
// One pass per TS step, in the TS order: splitting it would scatter the
// window bookkeeping across helpers used once.
#[allow(clippy::too_many_lines)]
pub fn build_learning_report(
    days: &[LearningDay],
    min_cohort_n: u64,
    now_ms: u64,
) -> LearningReport {
    let min_cohort_n = min_cohort_n.max(1);
    let mut sorted: Vec<&LearningDay> = days.iter().collect();
    sorted.sort_by(|left, right| left.day.cmp(&right.day));
    let mut commits: Vec<&crate::index::RefinementCommit> = sorted
        .iter()
        .flat_map(|day| day.commits.iter())
        .filter(|commit| !commit.addressed.is_empty())
        .collect();
    commits.sort_by(|left, right| pa_ravo::locale_compare(&left.at, &right.at));
    let mut report = LearningReport {
        generated_at: pa_ledger::iso_from_millis(now_ms),
        days: sorted.iter().map(|day| day.day.clone()).collect(),
        pivot_at: None,
        pivot_day: None,
        commits: commits.len(),
        turns_before: 0,
        turns_after: 0,
        fingerprints: Vec::new(),
        unobserved_treated: Vec::new(),
        treated: CohortStats::default(),
        untreated: CohortStats::default(),
        series: Vec::new(),
        u: None,
        p_value: None,
        insufficient_evidence: None,
        min_cohort_n,
    };
    let Some(pivot) = commits.get((commits.len().saturating_sub(1)) / 2) else {
        report.insufficient_evidence =
            Some("no refinement.committed record in the index".to_string());
        return report;
    };
    let pivot_day: String = pivot.at.chars().take(10).collect();
    let mut treated_ids: IndexMap<&str, ()> = IndexMap::new();
    for id in commits.iter().flat_map(|commit| commit.addressed.iter()) {
        treated_ids.insert(id.as_str(), ());
    }

    let mut before_counts: HashMap<&str, u64> = HashMap::new();
    let mut after_counts: HashMap<&str, u64> = HashMap::new();
    let mut labels: IndexMap<&str, (&str, &str)> = IndexMap::new();
    let mut per_day: Vec<(&LearningDay, Window, HashMap<&str, u64>)> = Vec::new();
    for day in &sorted {
        let window = match day.day.as_str().cmp(pivot_day.as_str()) {
            std::cmp::Ordering::Less => Window::Before,
            std::cmp::Ordering::Greater => Window::After,
            std::cmp::Ordering::Equal => Window::Pivot,
        };
        match window {
            Window::Before => report.turns_before += day.turns,
            Window::After => report.turns_after += day.turns,
            Window::Pivot => {}
        }
        let mut counts: HashMap<&str, u64> = HashMap::new();
        for stat in day.fingerprints.iter().filter(|stat| stat.failure) {
            let id = stat.fingerprint.as_str();
            labels.insert(
                id,
                (stat.name.as_str(), stat.message.as_deref().unwrap_or("")),
            );
            *counts.entry(id).or_insert(0) += stat.count;
            match window {
                Window::Before => *before_counts.entry(id).or_insert(0) += stat.count,
                Window::After => *after_counts.entry(id).or_insert(0) += stat.count,
                Window::Pivot => {}
            }
        }
        per_day.push((day, window, counts));
    }

    let mut fingerprints: Vec<FingerprintTrend> = Vec::new();
    for (id, (name, message)) in &labels {
        let before_count = before_counts.get(id).copied().unwrap_or(0);
        let after_count = after_counts.get(id).copied().unwrap_or(0);
        if before_count == 0 && after_count == 0 {
            continue;
        }
        let before_rate = rate(before_count, report.turns_before);
        let after_rate = rate(after_count, report.turns_after);
        fingerprints.push(FingerprintTrend {
            fingerprint: (*id).to_string(),
            name: (*name).to_string(),
            message: (*message).to_string(),
            treated: treated_ids.contains_key(id),
            before_count,
            after_count,
            before_rate,
            after_rate,
            delta: after_rate - before_rate,
        });
    }
    fingerprints.sort_by(|left, right| {
        left.delta
            .total_cmp(&right.delta)
            .then_with(|| pa_ravo::locale_compare(&left.fingerprint, &right.fingerprint))
    });

    let observed: HashSet<&str> = fingerprints
        .iter()
        .map(|trend| trend.fingerprint.as_str())
        .collect();
    let mut unobserved: Vec<String> = treated_ids
        .keys()
        .filter(|id| !observed.contains(*id))
        .map(|id| (*id).to_string())
        .collect();
    unobserved.sort();
    let treated: Vec<&FingerprintTrend> =
        fingerprints.iter().filter(|trend| trend.treated).collect();
    let untreated: Vec<&FingerprintTrend> =
        fingerprints.iter().filter(|trend| !trend.treated).collect();
    let treated = cohort(&treated);
    let untreated = cohort(&untreated);
    // The cohort denominators are the full membership, not the fingerprints
    // that happened to fire that day.
    let cohort_rate = |counts: &HashMap<&str, u64>, turns: u64, ids: &[String]| -> f64 {
        if ids.is_empty() {
            return 0.0;
        }
        let sum: u64 = ids
            .iter()
            .map(|id| counts.get(id.as_str()).copied().unwrap_or(0))
            .sum();
        #[expect(clippy::cast_precision_loss, reason = "cohort sizes")]
        let len = ids.len() as f64;
        rate(sum, turns) / len
    };
    report.series = per_day
        .iter()
        .map(|(day, window, counts)| LearningDayPoint {
            day: day.day.clone(),
            turns: day.turns,
            window: *window,
            treated_rate: cohort_rate(counts, day.turns, &treated.ids),
            untreated_rate: cohort_rate(counts, day.turns, &untreated.ids),
        })
        .collect();
    report.pivot_at = Some(pivot.at.clone());
    report.pivot_day = Some(pivot_day);
    report.fingerprints = fingerprints;
    report.unobserved_treated = unobserved;
    report.treated = treated;
    report.untreated = untreated;

    if report.turns_before == 0 || report.turns_after == 0 {
        let side = if report.turns_before == 0 {
            "before"
        } else {
            "after"
        };
        report.insufficient_evidence =
            Some(format!("no {TURN_SPAN_NAME} exposure {side} the pivot day"));
        return report;
    }
    let (treated_n, untreated_n) = (report.treated.n() as u64, report.untreated.n() as u64);
    if treated_n < min_cohort_n || untreated_n < min_cohort_n {
        report.insufficient_evidence = Some(format!(
            "cohorts are too small (treated {treated_n}, untreated {untreated_n}, minimum {min_cohort_n} each)"
        ));
        return report;
    }
    match mann_whitney_one_sided(&report.treated.deltas, &report.untreated.deltas) {
        Some(test) => {
            report.u = Some(test.u);
            report.p_value = Some(test.p_value);
        }
        None => {
            report.insufficient_evidence = Some(
                "every fingerprint changed by the same amount; the test has no variance"
                    .to_string(),
            );
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_agrees_with_the_platform_within_an_ulp() {
        for x in [
            -40.0, -9.5, -1.0, -0.3, 0.0, 1e-30, 0.2, 0.7, 1.0, 3.5, 100.0, 709.0, -745.0,
        ] {
            let (ours, platform) = (fdlibm_exp(x), f64::exp(x));
            assert!(
                ours.to_bits().abs_diff(platform.to_bits()) <= 1,
                "{x}: {ours} vs {platform}"
            );
        }
        assert_eq!(fdlibm_exp(f64::NEG_INFINITY).to_bits(), 0.0_f64.to_bits());
        assert!(fdlibm_exp(f64::NAN).is_nan());
    }

    #[test]
    fn mann_whitney_is_one_sided() {
        let low = [-7.0, -7.0, -7.0, -6.0, -6.0, -6.0, -5.0, -5.0, -5.0, -5.0];
        let high = [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0];
        let forward = mann_whitney_one_sided(&low, &high).unwrap();
        let reversed = mann_whitney_one_sided(&high, &low).unwrap();
        assert_eq!((forward.u, reversed.u), (0.0, 100.0));
        assert!(forward.p_value < 0.05);
        assert!(reversed.p_value > 0.95);
        assert_eq!(mann_whitney_one_sided(&[], &[1.0, 2.0]), None);
        assert_eq!(mann_whitney_one_sided(&[1.0], &[1.0]), None);
    }
}
