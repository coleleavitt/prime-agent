//! Circle packing in the unit square (TS `tasks/circle-packing.ts`).
//!
//! Choose `n` centers in `[0,1]^2` and radii `r_i >= 0`, every circle inside
//! the square and no two overlapping; maximize the sum of radii. The root is a
//! jittered grid; a proposal perturbs a few centers by a seeded gaussian and
//! assigns radii by the closed-form FEASIBLE repair
//! `r_i = ½ min(boundaryDist_i, min_{j≠i} dist(i, j))`.

use serde_json::{Value, json};

use crate::json::number;
use crate::rng::SeededRng;
use crate::task::{ArtifactShapeError, Evaluation, FailClass, ProposeParams, ScoredTask};

/// Tolerance for the independent validity recomputation.
pub const CIRCLE_PACKING_EPS: f64 = 1e-9;
/// How far, in grid-cell widths, the root centers are jittered off the grid.
const ROOT_JITTER: f64 = 1.8;

/// One packing.
#[derive(Debug, Clone, PartialEq)]
pub struct CirclePackingArtifact {
    pub n: usize,
    pub xs: Vec<f64>,
    pub ys: Vec<f64>,
    pub rs: Vec<f64>,
}

/// The circle-packing task for `n` circles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CirclePacking {
    n: usize,
}

impl CirclePacking {
    /// `n` must be at least 1 (the registry requires 2).
    #[must_use]
    pub fn new(n: usize) -> Self {
        Self { n: n.max(1) }
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn grid_shape(n: usize) -> (usize, usize) {
    let cols = ((n as f64).sqrt().ceil() as usize).max(1);
    let rows = n.div_ceil(cols).max(1);
    (cols, rows)
}

fn clamp_unit(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

fn repair_radii(xs: &[f64], ys: &[f64]) -> Vec<f64> {
    let n = xs.len();
    (0..n)
        .map(|i| {
            let (xi, yi) = (xs[i], ys[i]);
            let boundary = xi.min(1.0 - xi).min(yi).min(1.0 - yi);
            let mut nearest = f64::INFINITY;
            for j in 0..n {
                if j == i {
                    continue;
                }
                let dx = xi - xs[j];
                let dy = yi - ys[j];
                let dist = (dx * dx + dy * dy).sqrt();
                if dist < nearest {
                    nearest = dist;
                }
            }
            let limit = if nearest.is_finite() {
                boundary.min(nearest)
            } else {
                boundary
            };
            (0.5 * limit).max(0.0)
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn root_artifact(n: usize, rng: &mut SeededRng) -> CirclePackingArtifact {
    let (cols, rows) = grid_shape(n);
    let cell_w = 1.0 / cols as f64;
    let cell_h = 1.0 / rows as f64;
    let mut xs = Vec::with_capacity(n);
    let mut ys = Vec::with_capacity(n);
    for i in 0..n {
        let col = (i % cols) as f64;
        let row = (i / cols) as f64;
        let jitter_x = (rng.next() - 0.5) * ROOT_JITTER * cell_w;
        let jitter_y = (rng.next() - 0.5) * ROOT_JITTER * cell_h;
        xs.push(clamp_unit((col + 0.5) * cell_w + jitter_x));
        ys.push(clamp_unit((row + 0.5) * cell_h + jitter_y));
    }
    let rs = repair_radii(&xs, &ys);
    CirclePackingArtifact { n, xs, ys, rs }
}

fn perturb(
    parent: &CirclePackingArtifact,
    step_scale: f64,
    move_count: usize,
    rng: &mut SeededRng,
) -> CirclePackingArtifact {
    let n = parent.n;
    let mut xs = parent.xs.clone();
    let mut ys = parent.ys.clone();
    let count = move_count.min(n).max(1);
    for _ in 0..count {
        let i = rng.next_int(n);
        xs[i] = clamp_unit(xs[i] + rng.next_gaussian() * step_scale);
        ys[i] = clamp_unit(ys[i] + rng.next_gaussian() * step_scale);
    }
    let rs = repair_radii(&xs, &ys);
    CirclePackingArtifact { n, xs, ys, rs }
}

fn evaluate_artifact(artifact: &CirclePackingArtifact) -> Evaluation {
    let CirclePackingArtifact { n, xs, ys, rs } = artifact;
    let n = *n;
    if n < 1 || xs.len() != n || ys.len() != n || rs.len() != n {
        return Evaluation::invalid(FailClass::InvalidShape);
    }
    if (0..n).any(|i| !xs[i].is_finite() || !ys[i].is_finite() || !rs[i].is_finite()) {
        return Evaluation::invalid(FailClass::NonFinite);
    }
    let eps = CIRCLE_PACKING_EPS;
    for i in 0..n {
        let r = rs[i];
        if r < -eps {
            return Evaluation::invalid(FailClass::NegativeRadius);
        }
        let (x, y) = (xs[i], ys[i]);
        if x - r < -eps || x + r > 1.0 + eps || y - r < -eps || y + r > 1.0 + eps {
            return Evaluation::invalid(FailClass::OutOfBounds);
        }
    }
    for i in 0..n {
        for j in (i + 1)..n {
            let dx = xs[i] - xs[j];
            let dy = ys[i] - ys[j];
            let dist = (dx * dx + dy * dy).sqrt();
            if rs[i] + rs[j] > dist + eps {
                return Evaluation::invalid(FailClass::Overlap);
            }
        }
    }
    let sum: f64 = rs.iter().fold(0.0, |sum, r| sum + r);
    if !sum.is_finite() {
        return Evaluation::invalid(FailClass::NonFinite);
    }
    Evaluation::valid(sum)
}

fn number_array(
    value: Option<&Value>,
    length: usize,
    transform: fn(f64) -> f64,
) -> Result<Vec<f64>, ArtifactShapeError> {
    let Some(Value::Array(items)) = value else {
        return Err(ArtifactShapeError(
            "circle-packing artifact array has the wrong shape".to_string(),
        ));
    };
    if items.len() != length {
        return Err(ArtifactShapeError(
            "circle-packing artifact array has the wrong shape".to_string(),
        ));
    }
    items
        .iter()
        .map(|raw| {
            raw.as_f64()
                .filter(|raw| raw.is_finite())
                .map(transform)
                .ok_or_else(|| {
                    ArtifactShapeError(
                        "circle-packing artifact contains a non-finite value".to_string(),
                    )
                })
        })
        .collect()
}

/// Round a policy's `branchWidth` like `Math.round` and floor at 1.
pub(crate) fn move_count(params: &ProposeParams) -> usize {
    usize::try_from(params.branch_width.max(1)).unwrap_or(1)
}

/// `Math.max(1, Math.trunc(refineDepth))`.
pub(crate) fn refine_depth(params: &ProposeParams) -> usize {
    usize::try_from(params.refine_depth.max(1)).unwrap_or(1)
}

impl ScoredTask for CirclePacking {
    type Artifact = CirclePackingArtifact;

    fn id(&self) -> &'static str {
        "circle-packing"
    }

    fn root(&self, rng: &mut SeededRng) -> CirclePackingArtifact {
        root_artifact(self.n, rng)
    }

    fn propose(
        &self,
        parent: Option<&CirclePackingArtifact>,
        params: &ProposeParams,
        rng: &mut SeededRng,
        _round: u32,
    ) -> CirclePackingArtifact {
        let source = match parent {
            Some(parent) => parent.clone(),
            None => root_artifact(self.n, rng),
        };
        let moves = move_count(params);
        let mut best: Option<CirclePackingArtifact> = None;
        let mut best_score = f64::NEG_INFINITY;
        for _ in 0..refine_depth(params) {
            let candidate = perturb(&source, params.step_scale, moves, rng);
            let evaluation = evaluate_artifact(&candidate);
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
        best.unwrap_or_else(|| perturb(&source, params.step_scale, moves, rng))
    }

    fn evaluate(&self, candidate: &CirclePackingArtifact) -> Evaluation {
        evaluate_artifact(candidate)
    }

    fn serialize(&self, candidate: &CirclePackingArtifact) -> Value {
        let numbers = |values: &[f64]| Value::Array(values.iter().copied().map(number).collect());
        json!({
            "n": candidate.n,
            "xs": numbers(&candidate.xs),
            "ys": numbers(&candidate.ys),
            "rs": numbers(&candidate.rs),
        })
    }

    fn deserialize(&self, value: &Value) -> Result<CirclePackingArtifact, ArtifactShapeError> {
        let Value::Object(record) = value else {
            return Err(ArtifactShapeError(
                "circle-packing artifact must be an object".to_string(),
            ));
        };
        let count = record
            .get("n")
            .and_then(Value::as_f64)
            .filter(|count| count.fract() == 0.0 && *count >= 1.0)
            .ok_or_else(|| {
                ArtifactShapeError("circle-packing artifact has an invalid n".to_string())
            })?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let count = count as usize;
        let xs = number_array(record.get("xs"), count, clamp_unit)?;
        let ys = number_array(record.get("ys"), count, clamp_unit)?;
        let rs = number_array(record.get("rs"), count, |raw| raw.max(0.0))?;
        Ok(CirclePackingArtifact {
            n: count,
            xs,
            ys,
            rs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Seed;

    fn packing(xs: &[f64], ys: &[f64], rs: &[f64]) -> CirclePackingArtifact {
        CirclePackingArtifact {
            n: xs.len().max(ys.len()),
            xs: xs.to_vec(),
            ys: ys.to_vec(),
            rs: rs.to_vec(),
        }
    }

    #[test]
    fn every_refinement_stays_valid() {
        let task = CirclePacking::new(26);
        let params = ProposeParams {
            step_scale: 0.2,
            refine_depth: 2,
            branch_width: 2,
        };
        for seed in [1, 2, 3] {
            let mut current = task.root(&mut SeededRng::new(&Seed::Number(seed)));
            assert!(task.evaluate(&current).valid);
            let mut rng = SeededRng::new(&Seed::Number(seed * 31 + 5));
            for step in 1..=8 {
                current = task.propose(Some(&current), &params, &mut rng, step);
                let evaluation = task.evaluate(&current);
                assert!(
                    evaluation.valid
                        && evaluation.fail_class.is_none()
                        && evaluation.score.is_finite()
                );
            }
        }
    }

    #[test]
    fn scores_the_sum_of_radii_and_classifies_every_failure() {
        let two = CirclePacking::new(2);
        let valid = two.evaluate(&packing(&[0.2, 0.8], &[0.2, 0.8], &[0.15, 0.15]));
        assert!(valid.valid);
        assert!((valid.score - 0.3).abs() < 1e-12);
        assert_eq!(
            two.evaluate(&packing(&[0.5, 0.5], &[0.5, 0.5], &[0.3, 0.3])),
            Evaluation::invalid(FailClass::Overlap)
        );
        assert!(
            two.evaluate(&packing(&[0.3, 0.7], &[0.5, 0.5], &[0.2, 0.2]))
                .valid
        );
        assert!(
            !two.evaluate(&packing(
                &[0.3, 0.7],
                &[0.5, 0.5],
                &[0.2 + 10.0 * CIRCLE_PACKING_EPS, 0.2]
            ))
            .valid
        );
        let one = CirclePacking::new(1);
        let class = |artifact: CirclePackingArtifact| one.evaluate(&artifact).fail_class;
        assert_eq!(
            class(packing(&[0.95], &[0.5], &[0.2])),
            Some(FailClass::OutOfBounds)
        );
        assert_eq!(
            class(packing(&[0.5], &[0.5], &[-0.1])),
            Some(FailClass::NegativeRadius)
        );
        assert_eq!(
            class(packing(&[f64::NAN], &[0.5], &[0.1])),
            Some(FailClass::NonFinite)
        );
        assert_eq!(
            class(packing(&[0.5], &[0.5, 0.5], &[0.1, 0.1])),
            Some(FailClass::InvalidShape)
        );
    }

    #[test]
    fn round_trips_through_its_canonical_blob() {
        let task = CirclePacking::new(26);
        let root = task.root(&mut SeededRng::new(&Seed::Number(5)));
        let serialized = task.serialize(&root);
        let restored = task.deserialize(&serialized).expect("round trip");
        assert_eq!(
            crate::json::canonical_json(&task.serialize(&restored)),
            crate::json::canonical_json(&serialized)
        );
        assert!(
            task.deserialize(&json!({"n": 2, "xs": [0.1], "ys": [0.1, 0.2], "rs": [0.0, 0.0]}))
                .is_err()
        );
    }
}
