//! The per-run dreams log (TS `dreams.ts`): one JSONL line per candidate per
//! dreaming step, one step summary, and one probation line per adopted
//! policy, at `<dir>/dreams/<runKey>.jsonl` (directory 0700, file 0600,
//! created on the first step). The post-hoc final selection is logged with
//! iteration -1. Writing touches no rng and no tree.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::improve::{CandidateVerdict, DreamerKind, LeverScanRecord, evidence_trees_of};
use crate::json;
use crate::policy::{ExplorationPolicy, policy_id};
use crate::store::{DreamStoreError, append_private, create_dir_private};

/// Where a line came from when the run is an experiment arm.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DreamsLogContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm: Option<String>,
}

/// The probation rollout of an adopted policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DreamProbationRecord {
    pub policy_id: String,
    pub incumbent_policy_id: String,
    pub tree_id: String,
    pub round_best: f64,
    pub floor: f64,
    pub charged_probes: f64,
    pub charged_rounds: f64,
    pub incumbent_charged_probes: f64,
    pub incumbent_charged_rounds: f64,
    pub evidence_trees: usize,
    pub reverted: bool,
}

/// What one step contributes to the log.
pub struct DreamStepInput<'a> {
    pub iteration: i64,
    pub pool_size: usize,
    pub candidates: &'a [CandidateVerdict],
    pub current_score: f64,
    pub chosen_policy: &'a ExplorationPolicy,
    pub improved: bool,
    pub dreamer: DreamerKind,
    pub measured_trees: usize,
    pub lever_scan: Option<&'a LeverScanRecord>,
}

/// `<dir>/dreams`.
#[must_use]
pub fn dreams_dir(dir: &Path) -> PathBuf {
    dir.join("dreams")
}

/// `<dir>/dreams/<runKey>.jsonl`.
#[must_use]
pub fn dreams_path(dir: &Path, run_key: &str) -> PathBuf {
    dreams_dir(dir).join(format!("{run_key}.jsonl"))
}

/// Append-only writer for one run's dreams log.
pub struct DreamsLog<'a> {
    pub path: PathBuf,
    clock: &'a dyn Fn() -> u64,
    context: DreamsLogContext,
}

impl<'a> DreamsLog<'a> {
    #[must_use]
    pub fn new(path: PathBuf, clock: &'a dyn Fn() -> u64, context: DreamsLogContext) -> Self {
        Self {
            path,
            clock,
            context,
        }
    }

    /// `{type, ts, ...context, iteration}`: the head every line starts with.
    fn head(&self, line_type: &str, iteration: i64) -> Map<String, Value> {
        let mut line = Map::new();
        line.insert("type".to_string(), Value::from(line_type));
        line.insert("ts".to_string(), Value::from((self.clock)()));
        if let Some(experiment_id) = &self.context.experiment_id {
            line.insert(
                "experimentId".to_string(),
                Value::from(experiment_id.as_str()),
            );
        }
        if let Some(arm) = &self.context.arm {
            line.insert("arm".to_string(), Value::from(arm.as_str()));
        }
        line.insert("iteration".to_string(), Value::from(iteration));
        line
    }

    fn extend(line: &mut Map<String, Value>, record: &impl Serialize) {
        if let Ok(Value::Object(fields)) = serde_json::to_value(record) {
            for (key, value) in fields {
                line.insert(key, value);
            }
        }
    }

    /// Write every candidate line of a step, then its step line.
    ///
    /// # Errors
    ///
    /// [`DreamStoreError`] on a filesystem failure.
    pub fn record_step(&self, input: &DreamStepInput<'_>) -> Result<(), DreamStoreError> {
        let mut lines = Vec::with_capacity(input.candidates.len() + 1);
        for verdict in input.candidates {
            let mut line = self.head("candidate", input.iteration);
            Self::extend(&mut line, verdict);
            lines.push(Value::Object(line));
        }
        let mut step = self.head("step", input.iteration);
        step.insert("poolSize".to_string(), Value::from(input.pool_size));
        step.insert(
            "measuredTrees".to_string(),
            Value::from(input.measured_trees),
        );
        step.insert(
            "evidenceTrees".to_string(),
            Value::from(evidence_trees_of(input.measured_trees)),
        );
        step.insert(
            "currentValue".to_string(),
            json::number(input.current_score),
        );
        step.insert(
            "chosenPolicyId".to_string(),
            Value::from(policy_id(input.chosen_policy)),
        );
        step.insert("improved".to_string(), Value::from(input.improved));
        step.insert("dreamer".to_string(), Value::from(input.dreamer.as_str()));
        step.insert(
            "leverScan".to_string(),
            input
                .lever_scan
                .and_then(|scan| serde_json::to_value(scan).ok())
                .unwrap_or(Value::Null),
        );
        lines.push(Value::Object(step));
        self.write(&lines)
    }

    /// Write the probation line of an adopted policy, after its redeploy.
    ///
    /// # Errors
    ///
    /// [`DreamStoreError`] on a filesystem failure.
    pub fn record_probation(
        &self,
        iteration: i64,
        record: &DreamProbationRecord,
    ) -> Result<(), DreamStoreError> {
        let mut line = self.head("probation", iteration);
        Self::extend(&mut line, record);
        self.write(&[Value::Object(line)])
    }

    fn write(&self, lines: &[Value]) -> Result<(), DreamStoreError> {
        if let Some(parent) = self.path.parent() {
            create_dir_private(parent)?;
        }
        let mut text = String::new();
        for line in lines {
            text.push_str(&json::stringify(line));
            text.push('\n');
        }
        append_private(&self.path, &text)
    }
}

/// One parsed dreams-log line.
#[derive(Debug, Clone, PartialEq)]
pub enum DreamsLogLine {
    Candidate { iteration: i64, line: Value },
    Step { iteration: i64, line: Value },
    Probation { iteration: i64, line: Value },
}

fn is_well_formed(record: &Map<String, Value>) -> Option<&'static str> {
    let number = |key: &str| record.get(key).is_some_and(Value::is_number);
    let string = |key: &str| record.get(key).is_some_and(Value::is_string);
    let boolean = |key: &str| record.get(key).is_some_and(Value::is_boolean);
    if !number("ts") || !number("iteration") {
        return None;
    }
    match record.get("type").and_then(Value::as_str) {
        Some("candidate") => {
            let reason = record.get("reason").and_then(|reason| {
                serde_json::from_value::<crate::improve::CandidateReason>(reason.clone()).ok()
            });
            (number("index")
                && string("policyId")
                && record.get("policy").is_some_and(Value::is_object)
                && number("value")
                && number("quality")
                && boolean("eligible")
                && reason.is_some())
            .then_some("candidate")
        }
        Some("step") => (number("poolSize")
            && number("currentValue")
            && string("chosenPolicyId")
            && boolean("improved")
            && string("dreamer"))
        .then_some("step"),
        Some("probation") => (string("policyId")
            && string("incumbentPolicyId")
            && string("treeId")
            && number("roundBest")
            && number("floor")
            && boolean("reverted"))
        .then_some("probation"),
        _ => None,
    }
}

/// Parse a dreams log; a missing file is an empty log, a malformed line an error.
///
/// # Errors
///
/// [`DreamStoreError`] on a malformed line or an unreadable file.
pub fn read_dreams_log(path: &Path) -> Result<Vec<DreamsLogLine>, DreamStoreError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(DreamStoreError::io(path, error)),
    };
    let malformed =
        || DreamStoreError::Message(format!("malformed dreams log line in {}", path.display()));
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        if raw.trim().is_empty() {
            continue;
        }
        let value = json::parse(raw).map_err(|_| malformed())?;
        let Some(record) = value.as_object() else {
            return Err(malformed());
        };
        let kind = is_well_formed(record).ok_or_else(malformed)?;
        let iteration = record
            .get("iteration")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        lines.push(match kind {
            "candidate" => DreamsLogLine::Candidate {
                iteration,
                line: value,
            },
            "step" => DreamsLogLine::Step {
                iteration,
                line: value,
            },
            _ => DreamsLogLine::Probation {
                iteration,
                line: value,
            },
        });
    }
    Ok(lines)
}
