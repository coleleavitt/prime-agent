//! The first autocorrelation inequality (TS `tasks/autocorrelation.ts`).
//!
//! A step function of `n` non-negative bin weights on `[-1/4, 1/4]`,
//! normalized to integral 1; minimize the peak of its autoconvolution. The
//! score is `1 / peak`, computed EXACTLY as `h · max_k Σ_{i+j=k} w_i w_j` (the
//! autoconvolution of a step function is piecewise linear, so it peaks at a
//! knot). The uniform root scores 0.5.

use serde_json::{Value, json};

use super::circle_packing::{move_count, refine_depth};
use crate::json::number;
use crate::rng::SeededRng;
use crate::task::{ArtifactShapeError, Evaluation, FailClass, ProposeParams, ScoredTask};

/// The bin counts the paper studies.
pub const AUTOCORRELATION_BIN_COUNTS: [usize; 3] = [32, 64, 128];
/// The default bin count.
pub const DEFAULT_AUTOCORRELATION_N: usize = 64;
const SUPPORT_HALF_WIDTH: f64 = 0.25;
const UNIFORM_DENSITY: f64 = 2.0;
const MAX_WINDOW_RADIUS: usize = 3;

/// A weight vector.
#[derive(Debug, Clone, PartialEq)]
pub struct AutocorrelationArtifact {
    pub n: usize,
    pub weights: Vec<f64>,
}

/// The autocorrelation task with `n` bins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Autocorrelation {
    n: usize,
}

impl Autocorrelation {
    /// `n` must be at least 2 (the registry requires one of the paper's sizes).
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self { n: n.max(2) }
    }
}

/// Bin width `h = 1 / (2n)`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn bin_width(n: usize) -> f64 {
    (2.0 * SUPPORT_HALF_WIDTH) / n as f64
}

/// `(f * f)` at the `2n - 1` interior knots.
#[must_use]
pub fn autoconvolution_knots(weights: &[f64]) -> Vec<f64> {
    let n = weights.len();
    let h = bin_width(n);
    let mut knots = vec![0.0; (2 * n).saturating_sub(1)];
    for (i, wi) in weights.iter().enumerate() {
        if *wi == 0.0 {
            continue;
        }
        for (j, wj) in weights.iter().enumerate() {
            knots[i + j] += wi * wj;
        }
    }
    for knot in &mut knots {
        *knot *= h;
    }
    knots
}

/// `max_t (f * f)(t)`.
#[must_use]
pub fn autoconvolution_peak(weights: &[f64]) -> f64 {
    autoconvolution_knots(weights)
        .into_iter()
        .fold(0.0, |peak, value| if value > peak { value } else { peak })
}

/// Scale `weights` to integral 1; `None` when the integral is not positive and finite.
#[must_use]
pub fn normalize_weights(weights: &[f64]) -> Option<Vec<f64>> {
    let h = bin_width(weights.len());
    let sum: f64 = weights.iter().fold(0.0, |sum, value| sum + value);
    let integral = sum * h;
    if !integral.is_finite() || integral <= 0.0 {
        return None;
    }
    Some(weights.iter().map(|value| value / integral).collect())
}

fn evaluate_artifact(artifact: &AutocorrelationArtifact, n: usize) -> Evaluation {
    if artifact.n != n || artifact.weights.len() != n {
        return Evaluation::invalid(FailClass::InvalidShape);
    }
    if artifact.weights.iter().any(|value| !value.is_finite()) {
        return Evaluation::invalid(FailClass::NonFinite);
    }
    if artifact.weights.iter().any(|value| *value < 0.0) {
        return Evaluation::invalid(FailClass::NegativeWeight);
    }
    let Some(normalized) = normalize_weights(&artifact.weights) else {
        return Evaluation::invalid(FailClass::Degenerate);
    };
    let peak = autoconvolution_peak(&normalized);
    if !peak.is_finite() || peak <= 0.0 {
        return Evaluation::invalid(FailClass::NonFinite);
    }
    let score = 1.0 / peak;
    if !score.is_finite() {
        return Evaluation::invalid(FailClass::NonFinite);
    }
    Evaluation::valid(score)
}

fn perturb_bins(weights: &mut [f64], step_scale: f64, count: usize, rng: &mut SeededRng) {
    let n = weights.len();
    for _ in 0..count {
        let i = rng.next_int(n);
        weights[i] = (weights[i] + rng.next_gaussian() * step_scale * UNIFORM_DENSITY).max(0.0);
    }
}

fn move_mass(weights: &mut [f64], step_scale: f64, count: usize, rng: &mut SeededRng) {
    let n = weights.len();
    for _ in 0..count {
        let i = rng.next_int(n);
        let left = rng.next_int(2) == 0;
        // The neighbour in the drawn direction, or the other one at an edge.
        let j = if (left && i > 0) || (!left && i + 1 >= n) {
            i - 1
        } else {
            i + 1
        };
        let delta = step_scale.min(1.0) * rng.next() * weights[i];
        weights[i] -= delta;
        weights[j] += delta;
    }
}

#[allow(clippy::cast_precision_loss)]
fn smooth_or_sharpen(weights: &mut [f64], step_scale: f64, rng: &mut SeededRng) {
    let n = weights.len();
    let center = rng.next_int(n);
    let radius = 1 + rng.next_int(MAX_WINDOW_RADIUS);
    let sign = if rng.next_int(2) == 0 { 1.0 } else { -1.0 };
    let alpha = step_scale.min(1.0);
    let lo = center.saturating_sub(radius);
    let hi = (n - 1).min(center + radius);
    let sum: f64 = weights[lo..=hi].iter().fold(0.0, |sum, value| sum + value);
    let average = sum / (hi - lo + 1) as f64;
    for weight in &mut weights[lo..=hi] {
        *weight = (*weight + sign * alpha * (average - *weight)).max(0.0);
    }
}

fn mutate(
    parent: &AutocorrelationArtifact,
    step_scale: f64,
    moves: usize,
    rng: &mut SeededRng,
) -> AutocorrelationArtifact {
    let n = parent.n;
    let base = normalize_weights(&parent.weights).unwrap_or_else(|| vec![UNIFORM_DENSITY; n]);
    let mut weights = base.clone();
    let operation = rng.next_int(6);
    if operation < 3 {
        perturb_bins(&mut weights, step_scale, moves, rng);
    } else if operation < 5 {
        move_mass(&mut weights, step_scale, moves, rng);
    } else {
        smooth_or_sharpen(&mut weights, step_scale, rng);
    }
    AutocorrelationArtifact {
        n,
        weights: normalize_weights(&weights).unwrap_or(base),
    }
}

/// The public contract an LLM proposer is shown (never hidden data).
#[must_use]
pub fn autocorrelation_prompt_context(n: Option<usize>) -> String {
    let bins = n.map_or_else(|| "n".to_string(), |n| n.to_string());
    let last = n.map_or_else(|| "n-1".to_string(), |n| (n - 1).to_string());
    let width = n.map_or_else(|| "1/(2n)".to_string(), |n| format!("1/{}", 2 * n));
    let yours = n.map_or_else(
        || "yours must keep the current candidate's \"n\" and have exactly n weights".to_string(),
        |n| format!("yours must have \"n\": {n} and exactly {n} weights"),
    );
    [
        "Task: propose a step function f on [-1/4, 1/4] whose autoconvolution peak max_t (f*f)(t) is as SMALL as possible.".to_string(),
        "Contract:".to_string(),
        format!("- The candidate is {{\"n\": {bins}, \"weights\": [w_0, ..., w_{last}]}}: {bins} bins of width h = {width} covering [-1/4, 1/4] left to right; w_i is the density on bin i."),
        "- Every weight must be a finite number >= 0 and at least one must be positive. The evaluator rescales the weights so the integral sum_i w_i * h is exactly 1, so only the shape matters.".to_string(),
        "- The current candidate's \"peak\" field is its max_t (f*f)(t) after that rescaling (the uniform density has peak 2); the score is 1 / peak, higher is better, and a candidate must LOWER the peak to improve.".to_string(),
        "- The autoconvolution of a step function is piecewise linear and is evaluated exactly at its knots; there is no sampling to exploit.".to_string(),
        "Public hints:".to_string(),
        "- Moving mass from the middle toward both edges lowers the central peak (f*f)(0) at the cost of raising the shoulders; the optimum balances a wide flat top of the autoconvolution.".to_string(),
        "- Good known solutions are not the uniform density: they look like an asymmetric plateau, with a spike near one edge and a gentle taper toward the other.".to_string(),
        "- Small local edits (a few bins at a time) that keep the autoconvolution's top flat tend to help; a single dominant bin makes the peak grow with n and is the worst shape.".to_string(),
        format!("Exact output shape: {{\"n\": {bins}, \"weights\": [w_0, ..., w_{last}]}}, a JSON object with exactly these two keys (no \"peak\"). For example with n = 4: {{\"n\": 4, \"weights\": [1.5, 2.5, 2.5, 1.5]}}; {yours}."),
        format!("Return the complete candidate object; its weights array must have exactly {bins} entries."),
    ]
    .join("\n")
}

impl ScoredTask for Autocorrelation {
    type Artifact = AutocorrelationArtifact;

    fn id(&self) -> &'static str {
        "autocorrelation"
    }

    fn root(&self, _rng: &mut SeededRng) -> AutocorrelationArtifact {
        AutocorrelationArtifact {
            n: self.n,
            weights: vec![UNIFORM_DENSITY; self.n],
        }
    }

    fn propose(
        &self,
        parent: Option<&AutocorrelationArtifact>,
        params: &ProposeParams,
        rng: &mut SeededRng,
        _round: u32,
    ) -> AutocorrelationArtifact {
        let source = match parent {
            Some(parent) => parent.clone(),
            None => self.root(rng),
        };
        let moves = move_count(params);
        let mut best: Option<AutocorrelationArtifact> = None;
        let mut best_score = f64::NEG_INFINITY;
        for _ in 0..refine_depth(params) {
            let candidate = mutate(&source, params.step_scale, moves, rng);
            let evaluation = evaluate_artifact(&candidate, self.n);
            let value = if evaluation.valid {
                evaluation.score
            } else {
                f64::NEG_INFINITY
            };
            if best.is_none() || value > best_score {
                best = Some(candidate);
                best_score = value;
            }
        }
        best.unwrap_or_else(|| mutate(&source, params.step_scale, moves, rng))
    }

    fn evaluate(&self, candidate: &AutocorrelationArtifact) -> Evaluation {
        evaluate_artifact(candidate, self.n)
    }

    fn serialize(&self, candidate: &AutocorrelationArtifact) -> Value {
        let peak = normalize_weights(&candidate.weights).map_or(Value::Null, |normalized| {
            number(autoconvolution_peak(&normalized))
        });
        json!({
            "n": candidate.n,
            "peak": peak,
            "weights": Value::Array(candidate.weights.iter().copied().map(number).collect()),
        })
    }

    fn deserialize(&self, value: &Value) -> Result<AutocorrelationArtifact, ArtifactShapeError> {
        let Value::Object(record) = value else {
            return Err(ArtifactShapeError(
                "autocorrelation artifact must be an object".to_string(),
            ));
        };
        let n = self.n;
        if let Some(raw_n) = record.get("n") {
            #[allow(clippy::cast_precision_loss)]
            if raw_n.as_f64() != Some(n as f64) {
                return Err(ArtifactShapeError(format!(
                    "autocorrelation artifact must have n = {n}"
                )));
            }
        }
        let shape = || {
            ArtifactShapeError(format!(
                "autocorrelation artifact must have a weights array of length {n}"
            ))
        };
        let Some(Value::Array(items)) = record.get("weights") else {
            return Err(shape());
        };
        if items.len() != n {
            return Err(shape());
        }
        let weights = items
            .iter()
            .map(|raw| {
                raw.as_f64()
                    .filter(|raw| raw.is_finite())
                    .map(|raw| raw.max(0.0))
                    .ok_or_else(|| {
                        ArtifactShapeError(
                            "autocorrelation artifact contains a non-finite weight".to_string(),
                        )
                    })
            })
            .collect::<Result<Vec<f64>, _>>()?;
        Ok(AutocorrelationArtifact { n, weights })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uniform_root_scores_one_half_and_spikes_score_the_exact_peak() {
        let task = Autocorrelation::new(64);
        let mut rng = SeededRng::new(&crate::rng::Seed::Number(1));
        let root = task.root(&mut rng);
        assert!((task.evaluate(&root).score - 0.5).abs() < 1e-12);
        // Two spikes a and b at bins 10 and 40: c_k peaks at max(a², b², 2ab) on the integral-1 scale.
        let (a, b) = (3.0f64, 1.0f64);
        let mut weights = vec![0.0; 64];
        weights[10] = a;
        weights[40] = b;
        let h = bin_width(64);
        let integral = (a + b) * h;
        let (na, nb) = (a / integral, b / integral);
        let expected_peak = h * (na * na).max(nb * nb).max(2.0 * na * nb);
        let evaluation = task.evaluate(&AutocorrelationArtifact { n: 64, weights });
        assert!((evaluation.score - 1.0 / expected_peak).abs() < 1e-9);
        assert_eq!(
            task.evaluate(&AutocorrelationArtifact {
                n: 64,
                weights: vec![0.0; 64]
            }),
            Evaluation::invalid(FailClass::Degenerate)
        );
        let params = ProposeParams {
            step_scale: 0.2,
            refine_depth: 2,
            branch_width: 2,
        };
        let child = task.propose(Some(&root), &params, &mut rng, 1);
        let integral: f64 = child.weights.iter().sum::<f64>() * h;
        assert!((integral - 1.0).abs() < 1e-12);
        assert!(task.deserialize(&json!({"n": 32, "weights": []})).is_err());
    }
}
