//! Sum-difference (TS `tasks/sum-difference.ts`): for a finite integer set A,
//! maximize `Γ(A) = log(|A+A| / |A|) / log(|A−A| / |A|)`. Edits are seeded
//! add/remove/replace within `[-40, 40]` keeping `|A| >= 2`; degenerate sets
//! score `valid:false / 0`, never NaN.

use std::collections::HashSet;

use serde_json::{Value, json};

use super::circle_packing::refine_depth;
use crate::rng::SeededRng;
use crate::task::{ArtifactShapeError, Evaluation, FailClass, ProposeParams, ScoredTask};

const WINDOW: i64 = 40;
const WINDOW_SPAN: usize = 81;
const INITIAL_SIZE: usize = 8;
const MIN_SIZE: usize = 2;

/// A sorted set of distinct integers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumDifferenceArtifact {
    pub set: Vec<i64>,
}

/// The sum-difference task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SumDifference;

fn window_value(rng: &mut SeededRng) -> i64 {
    i64::try_from(rng.next_int(WINDOW_SPAN)).unwrap_or(0) - WINDOW
}

/// A JS `Set<number>`: distinct values in insertion order.
#[derive(Debug, Clone, Default)]
struct InsertionSet(Vec<i64>);

impl InsertionSet {
    fn add(&mut self, value: i64) {
        if !self.0.contains(&value) {
            self.0.push(value);
        }
    }

    fn has(&self, value: i64) -> bool {
        self.0.contains(&value)
    }

    fn delete(&mut self, value: i64) {
        self.0.retain(|known| *known != value);
    }

    fn size(&self) -> usize {
        self.0.len()
    }

    fn sorted(&self) -> Vec<i64> {
        let mut values = self.0.clone();
        values.sort_unstable();
        values
    }
}

fn root_artifact(rng: &mut SeededRng) -> SumDifferenceArtifact {
    let mut values = InsertionSet::default();
    let mut guard = 0;
    while values.size() < INITIAL_SIZE && guard < INITIAL_SIZE * 16 {
        values.add(window_value(rng));
        guard += 1;
    }
    while values.size() < MIN_SIZE {
        values.add(i64::try_from(values.size()).unwrap_or(0));
    }
    SumDifferenceArtifact {
        set: values.sorted(),
    }
}

fn add_value(current: &mut InsertionSet, rng: &mut SeededRng) {
    for _ in 0..16 {
        let value = window_value(rng);
        if !current.has(value) {
            current.add(value);
            return;
        }
    }
}

fn remove_value(current: &mut InsertionSet, rng: &mut SeededRng) {
    if current.size() <= MIN_SIZE {
        return;
    }
    let value = current.0[rng.next_int(current.size())];
    current.delete(value);
}

fn propose_edit(parent: &SumDifferenceArtifact, rng: &mut SeededRng) -> SumDifferenceArtifact {
    let mut current = InsertionSet::default();
    for value in &parent.set {
        current.add(*value);
    }
    match rng.next_int(3) {
        0 => add_value(&mut current, rng),
        1 => remove_value(&mut current, rng),
        _ => {
            remove_value(&mut current, rng);
            add_value(&mut current, rng);
        }
    }
    while current.size() < MIN_SIZE {
        add_value(&mut current, rng);
    }
    SumDifferenceArtifact {
        set: current.sorted(),
    }
}

#[allow(clippy::cast_precision_loss)]
fn evaluate_artifact(artifact: &SumDifferenceArtifact) -> Evaluation {
    let set = &artifact.set;
    let size = set.iter().collect::<HashSet<_>>().len();
    if size < MIN_SIZE {
        return Evaluation::invalid(FailClass::Degenerate);
    }
    let mut sums = HashSet::new();
    let mut diffs = HashSet::new();
    for a in set {
        for b in set {
            sums.insert(a + b);
            diffs.insert(a - b);
        }
    }
    let size = size as f64;
    let numerator = crate::js_math::log(sums.len() as f64 / size);
    let denominator = crate::js_math::log(diffs.len() as f64 / size);
    if !denominator.is_finite() || denominator == 0.0 {
        return Evaluation::invalid(FailClass::Degenerate);
    }
    let gamma = numerator / denominator;
    if !gamma.is_finite() {
        return Evaluation::invalid(FailClass::NonFinite);
    }
    Evaluation::valid(gamma)
}

impl ScoredTask for SumDifference {
    type Artifact = SumDifferenceArtifact;

    fn id(&self) -> &'static str {
        "sum-difference"
    }

    fn root(&self, rng: &mut SeededRng) -> SumDifferenceArtifact {
        root_artifact(rng)
    }

    fn propose(
        &self,
        parent: Option<&SumDifferenceArtifact>,
        params: &ProposeParams,
        rng: &mut SeededRng,
        _round: u32,
    ) -> SumDifferenceArtifact {
        let source = match parent {
            Some(parent) => parent.clone(),
            None => root_artifact(rng),
        };
        let mut best: Option<SumDifferenceArtifact> = None;
        let mut best_score = f64::NEG_INFINITY;
        for _ in 0..refine_depth(params) {
            let candidate = propose_edit(&source, rng);
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
        best.unwrap_or_else(|| propose_edit(&source, rng))
    }

    fn evaluate(&self, candidate: &SumDifferenceArtifact) -> Evaluation {
        evaluate_artifact(candidate)
    }

    fn serialize(&self, candidate: &SumDifferenceArtifact) -> Value {
        json!({ "set": candidate.set })
    }

    fn deserialize(&self, value: &Value) -> Result<SumDifferenceArtifact, ArtifactShapeError> {
        let Value::Object(record) = value else {
            return Err(ArtifactShapeError(
                "sum-difference artifact must be an object".to_string(),
            ));
        };
        let Some(Value::Array(raw)) = record.get("set") else {
            return Err(ArtifactShapeError(
                "sum-difference artifact must have a set array".to_string(),
            ));
        };
        let mut values = InsertionSet::default();
        for entry in raw {
            let Some(number) = entry.as_f64().filter(|number| number.is_finite()) else {
                return Err(ArtifactShapeError(
                    "sum-difference set contains a non-finite value".to_string(),
                ));
            };
            #[allow(clippy::cast_possible_truncation)]
            values.add(number.trunc() as i64);
        }
        Ok(SumDifferenceArtifact {
            set: values.sorted(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_gamma_and_marks_degenerate_sets_invalid() {
        let task = SumDifference;
        // A = {0, 1, 3}: A+A = {0,1,2,3,4,6} (6), A-A = {-3..3} (7).
        let expected = (6.0f64 / 3.0).ln() / (7.0f64 / 3.0).ln();
        let evaluation = task.evaluate(&SumDifferenceArtifact { set: vec![0, 1, 3] });
        assert!(evaluation.valid);
        assert!((evaluation.score - expected).abs() < 1e-12);
        assert_eq!(
            task.evaluate(&SumDifferenceArtifact { set: vec![5] }),
            Evaluation::invalid(FailClass::Degenerate)
        );
        assert_eq!(
            task.deserialize(&json!({"set": [3.7, -1, 3]})),
            Ok(SumDifferenceArtifact { set: vec![-1, 3] })
        );
    }
}
