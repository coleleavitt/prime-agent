//! Task-success checking: the parent's checkable ANSWER against the saved status ledger
//! (factory arm) or the parent's own collect dump (baseline arm).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    DEFAULT_WIDTH,
    ParsedAnswer,
    ReferenceFactory,
    ReferenceFactoryKind,
    TASK_MARKER_A,
    TASK_MARKER_B,
    builder_marker,
    parse_fenced_json,
    review_issue_ids,
};

/// Which arm a task check scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvalArm {
    Factory,
    Baseline,
}

impl EvalArm {
    /// The report row token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Factory => "factory",
            Self::Baseline => "baseline",
        }
    }
}

/// Join the baseline collect dump's values into one searchable text; `None`
/// when the dump is absent. Strings, numbers, and arrays join recursively
/// (the accepted review fix: the pr-manager dump's `fix_answers` array must
/// contribute its answers, and a numeric field like `rounds` cross-checks),
/// so a planted id the children never reported cannot hide inside an array.
#[must_use]
fn baseline_ledger_text(baseline_ledger: Option<&Value>) -> Option<String> {
    fn walk(value: &Value, parts: &mut Vec<String>) {
        match value {
            Value::String(text) => parts.push(text.clone()),
            Value::Number(number) => parts.push(number.to_string()),
            Value::Array(items) => items.iter().for_each(|item| walk(item, parts)),
            _ => {}
        }
    }
    let baseline = baseline_ledger?.as_object()?;
    let mut parts: Vec<String> = Vec::new();
    baseline.values().for_each(|value| walk(value, &mut parts));
    Some(parts.join("\n"))
}

fn ledger_node<'a>(ledger: &'a Value, id: &str) -> Option<&'a Value> {
    ledger
        .get("nodes")?
        .as_array()?
        .iter()
        .find(|node| node.get("id").and_then(Value::as_str) == Some(id))
}

/// A node's string `field`.
fn node_str<'a>(node: Option<&'a Value>, field: &str) -> Option<&'a str> {
    node.and_then(|node| node.get(field))
        .and_then(Value::as_str)
}

/// Whether the ledger holds an event of `kind` whose `field` is `value`.
fn has_event(ledger: &Value, kind: &str, field: &str, value: &str) -> bool {
    ledger
        .get("events")
        .and_then(Value::as_array)
        .is_some_and(|events| {
            events.iter().any(|event| {
                event.get("kind").and_then(Value::as_str) == Some(kind)
                    && event.get(field).and_then(Value::as_str) == Some(value)
            })
        })
}

/// What a trial's ANSWER is cross-checked against.
enum Evidence<'a> {
    /// The factory arm: the saved status ledger, when one was captured.
    Ledger(Option<&'a Value>),
    /// The baseline arm: the parent's collect dump as searchable text, when present.
    BaselineDump(Option<String>),
}

/// Check the parent's ANSWER against the factory's checkable answer. The
/// factory arm cross-checks the saved status ledger; the baseline arm
/// instead cross-checks the parent's own collect dump against the ANSWER
/// ids, so a baseline trial cannot pass on a self-reported ANSWER the
/// children never produced.
#[must_use]
pub fn check_task_success(
    factory: &ReferenceFactory,
    answer: Option<&ParsedAnswer>,
    ledger: Option<&Value>,
    arm: EvalArm,
    baseline_ledger: Option<&Value>,
) -> TaskCheckOutcome {
    let Some(answer) = answer else {
        return TaskCheckOutcome {
            ok: false,
            problems: vec!["no ANSWER line in the parent's final text".to_string()],
        };
    };
    let evidence = match arm {
        EvalArm::Factory => Evidence::Ledger(ledger),
        EvalArm::Baseline => Evidence::BaselineDump(baseline_ledger_text(baseline_ledger)),
    };
    let mut check = TaskCheck {
        answer,
        problems: Vec::new(),
    };
    match factory.kind {
        ReferenceFactoryKind::ReviewSweep => check.review_sweep(&evidence),
        ReferenceFactoryKind::Builder => {
            check.builder(factory.width.unwrap_or(DEFAULT_WIDTH), &evidence);
        }
        ReferenceFactoryKind::ResidentWatcher => check.resident_watcher(&evidence),
        ReferenceFactoryKind::PrManager => check.pr_manager(&evidence),
        // The escalation probe reads the saved ledger on either arm.
        ReferenceFactoryKind::ReviewSweepFail => check.review_sweep_fail(ledger),
        ReferenceFactoryKind::DryRunReject => check.dry_run_reject(),
    }
    TaskCheckOutcome {
        ok: check.problems.is_empty(),
        problems: check.problems,
    }
}

/// One ANSWER under the task-success check, and the problems found.
struct TaskCheck<'a> {
    answer: &'a ParsedAnswer,
    problems: Vec<String>,
}

impl TaskCheck<'_> {
    /// Each `required` id appears in the ANSWER's `listed` ids.
    fn require_listed(&mut self, listed: &[String], required: &[&str], label: &str) {
        for id in required {
            if !listed.iter().any(|known| known == id) {
                self.problems
                    .push(format!("{label}{id} missing from the ANSWER line"));
            }
        }
    }

    /// Each `required` id is mentioned in `text` (absent text mentions none).
    fn require_mentioned(
        &mut self,
        text: Option<&str>,
        required: &[&str],
        label: &str,
        place: &str,
    ) {
        for id in required {
            if !text.is_some_and(|text| text.contains(id)) {
                self.problems
                    .push(format!("{label}{id} missing from {place}"));
            }
        }
    }

    /// The baseline arm's cross-check: the collect dump mentions every `required` id and
    /// every id the ANSWER reported (`answered`, named `noun` in the problem).
    fn require_baseline_dump(
        &mut self,
        text: Option<&str>,
        required: &[&str],
        label: &str,
        answered: &[String],
        noun: &str,
    ) {
        let Some(text) = text else {
            self.problems.push(
                "baseline collect ledger missing (cannot verify the ANSWER against the children)"
                    .to_string(),
            );
            return;
        };
        self.require_mentioned(Some(text), required, label, "the baseline collect ledger");
        for id in answered {
            if !text.contains(id.as_str()) {
                self.problems.push(format!(
                    "ANSWER {noun} {id} is not present in the baseline collect ledger"
                ));
            }
        }
    }

    /// An ANSWER string field (`label`) holds `expected`.
    fn expect_answer(&mut self, label: &str, actual: Option<&str>, expected: &str) {
        if actual != Some(expected) {
            self.problems.push(format!(
                "ANSWER {label} is {}, expected {expected}",
                actual.unwrap_or("unset")
            ));
        }
    }

    /// The ledger's run state is `expected`.
    fn expect_ledger_state(&mut self, ledger: &Value, expected: &str) {
        if ledger["state"].as_str() != Some(expected) {
            self.problems.push(format!(
                "ledger state is {}, expected {expected}",
                ledger["state"].as_str().unwrap_or("unset")
            ));
        }
    }

    /// The node's string `field` is `expected`; `problem` otherwise.
    fn expect_node(&mut self, node: Option<&Value>, field: &str, expected: &str, problem: &str) {
        if node_str(node, field) != Some(expected) {
            self.problems.push(problem.to_string());
        }
    }

    /// The node's integer `field` (`label` in the problem) is `expected`.
    fn expect_node_int(&mut self, node: Option<&Value>, field: &str, expected: i64, label: &str) {
        let actual = node
            .and_then(|node| node.get(field))
            .and_then(Value::as_i64);
        if actual != Some(expected) {
            self.problems.push(format!(
                "{label} is {}, expected {expected}",
                actual.map_or_else(|| "unset".to_string(), |value| value.to_string())
            ));
        }
    }

    /// The review sweep: every planted issue reported and backed by the report node (or the
    /// baseline dump).
    fn review_sweep(&mut self, evidence: &Evidence<'_>) {
        let answer = self.answer;
        let issues = review_issue_ids();
        self.require_listed(&answer.issues, &issues, "planted issue ");
        match evidence {
            Evidence::BaselineDump(text) => self.require_baseline_dump(
                text.as_deref(),
                &issues,
                "planted issue ",
                &answer.issues,
                "issue",
            ),
            Evidence::Ledger(ledger) => {
                self.expect_answer("state", answer.state.as_deref(), "done");
                let Some(ledger) = ledger else {
                    return;
                };
                self.expect_ledger_state(ledger, "done");
                let report = ledger_node(ledger, "report");
                self.expect_node(report, "status", "done", "ledger report node is not done");
                self.require_mentioned(
                    node_str(report, "answer_preview"),
                    &issues,
                    "planted issue ",
                    "the report node answer preview",
                );
            }
        }
    }

    /// The builder: every node's marker reported and backed by the collector (or the
    /// baseline dump).
    fn builder(&mut self, width: u64, evidence: &Evidence<'_>) {
        let answer = self.answer;
        let markers: Vec<String> = (1..=width).map(builder_marker).collect();
        let markers: Vec<&str> = markers.iter().map(String::as_str).collect();
        self.require_listed(&answer.markers, &markers, "");
        match evidence {
            Evidence::BaselineDump(text) => {
                self.require_baseline_dump(
                    text.as_deref(),
                    &markers,
                    "",
                    &answer.markers,
                    "marker",
                );
            }
            Evidence::Ledger(ledger) => {
                self.expect_answer("state", answer.state.as_deref(), "done");
                let Some(ledger) = ledger else {
                    return;
                };
                self.expect_ledger_state(ledger, "done");
                let collector = ledger_node(ledger, "collector");
                self.expect_node(
                    collector,
                    "status",
                    "done",
                    "ledger collector node is not done",
                );
                self.require_mentioned(
                    node_str(collector, "answer_preview"),
                    &markers,
                    "",
                    "the collector answer preview",
                );
            }
        }
    }

    /// The resident watcher: both task markers reported, the watcher stopped and cancelled.
    fn resident_watcher(&mut self, evidence: &Evidence<'_>) {
        let answer = self.answer;
        let markers = [TASK_MARKER_A, TASK_MARKER_B];
        self.require_listed(&answer.markers, &markers, "");
        if !answer.stopped.iter().any(|known| known == "watcher") {
            self.problems
                .push("ANSWER does not report the resident watcher as stopped".to_string());
        }
        let ledger = match evidence {
            Evidence::BaselineDump(text) => {
                self.require_baseline_dump(
                    text.as_deref(),
                    &markers,
                    "",
                    &answer.markers,
                    "marker",
                );
                return;
            }
            Evidence::Ledger(None) => return,
            Evidence::Ledger(Some(ledger)) => ledger,
        };
        self.expect_ledger_state(ledger, "stopped");
        for id in ["task-a", "task-b"] {
            self.expect_node(
                ledger_node(ledger, id),
                "status",
                "done",
                &format!("ledger {id} node is not done"),
            );
        }
        let watcher = ledger_node(ledger, "watcher");
        self.expect_node(
            watcher,
            "status",
            "cancelled",
            "ledger watcher node is not cancelled",
        );
        let watcher_cancelled = watcher
            .and_then(|node| node.get("instances"))
            .and_then(Value::as_array)
            .is_some_and(|instances| {
                instances.iter().any(|instance| {
                    instance.get("status").and_then(Value::as_str) == Some("cancelled")
                })
            });
        if !watcher_cancelled {
            self.problems
                .push("ledger watcher instance is not cancelled".to_string());
        }
    }

    /// The pr-manager machine: approved after two review rounds with every planted defect
    /// fixed, and the resident monitoring state stopped.
    fn pr_manager(&mut self, evidence: &Evidence<'_>) {
        let answer = self.answer;
        if answer.approved != Some(true) {
            self.problems.push(format!(
                "ANSWER approved is {}, expected yes",
                answer
                    .approved
                    .map_or_else(|| "unset".to_string(), |approved| approved.to_string())
            ));
        }
        if answer.rounds != Some(2) {
            self.problems.push(format!(
                "ANSWER rounds is {}, expected 2",
                answer
                    .rounds
                    .map_or_else(|| "unset".to_string(), |rounds| rounds.to_string())
            ));
        }
        let issues = review_issue_ids();
        self.require_listed(&answer.defects, &issues, "planted issue ");
        if !answer.stopped.iter().any(|known| known == "monitoring") {
            self.problems.push(
                "ANSWER does not report the resident monitoring state as stopped".to_string(),
            );
        }
        match evidence {
            Evidence::BaselineDump(text) => {
                self.require_baseline_dump(
                    text.as_deref(),
                    &issues,
                    "planted issue ",
                    &answer.defects,
                    "defect",
                );
                if let (Some(text), Some(rounds)) = (text, answer.rounds) {
                    if !text.contains(&rounds.to_string()) {
                        self.problems.push(format!(
                            "ANSWER rounds {rounds} is not present in the baseline collect ledger"
                        ));
                    }
                }
            }
            Evidence::Ledger(Some(ledger)) => self.pr_manager_ledger(ledger, &issues),
            Evidence::Ledger(None) => {}
        }
    }

    /// The pr-manager's saved ledger: the review loop's length and verdict, the fix ledger,
    /// and the cancelled resident monitoring state.
    fn pr_manager_ledger(&mut self, ledger: &Value, issues: &[&str]) {
        self.expect_ledger_state(ledger, "stopped");
        let reviewing = ledger_node(ledger, "reviewing");
        let fixing = ledger_node(ledger, "fixing");
        let monitoring = ledger_node(ledger, "monitoring");
        // rounds is the review-loop length: the reviewing state's
        // entries_used.
        self.expect_node_int(
            reviewing,
            "entries_used",
            2,
            "ledger reviewing entries_used",
        );
        self.expect_node_int(fixing, "entries_used", 1, "ledger fixing entries_used");
        self.expect_node(
            reviewing,
            "status",
            "done",
            "ledger reviewing state is not done",
        );
        // The final verdict must be a fenced json block (multi-line
        // objects parse: a pretty-printed verdict is a valid
        // verdict) with approved === true.
        let approved = parse_fenced_json(node_str(reviewing, "answer_preview").unwrap_or_default())
            .as_ref()
            .and_then(|object| object.get("verdict"))
            .and_then(Value::as_object)
            .and_then(|verdict| verdict.get("approved"))
            .cloned();
        if approved != Some(Value::Bool(true)) {
            self.problems
                .push("final reviewing verdict is not approved true".to_string());
        }
        self.expect_node_int(reviewing, "max_entries", 4, "ledger reviewing max_entries");
        // The fix ledger: the fixing node's captured previews plus
        // every fixing answer_captured event must carry all planted
        // defect ids.
        let mut fix_texts: Vec<&str> = vec![node_str(fixing, "answer_preview").unwrap_or_default()];
        if let Some(events) = ledger.get("events").and_then(Value::as_array) {
            fix_texts.extend(
                events
                    .iter()
                    .filter(|event| {
                        event.get("kind").and_then(Value::as_str) == Some("answer_captured")
                            && event.get("node").and_then(Value::as_str) == Some("fixing")
                    })
                    .map(|event| {
                        event
                            .get("answer")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                    }),
            );
        }
        self.require_mentioned(
            Some(fix_texts.join("\n").as_str()),
            issues,
            "planted issue ",
            "the fixing ledger",
        );
        self.expect_node(
            monitoring,
            "lifecycle",
            "resident",
            "ledger monitoring state is not resident",
        );
        self.expect_node(
            monitoring,
            "status",
            "cancelled",
            "ledger monitoring state is not cancelled",
        );
    }

    /// The escalation probe: the run pauses on the broken reviewer and never starts the
    /// report.
    fn review_sweep_fail(&mut self, ledger: Option<&Value>) {
        let answer = self.answer;
        self.expect_answer("state", answer.state.as_deref(), "paused");
        self.expect_answer(
            "failed node",
            answer.failed_node.as_deref(),
            "review-broken",
        );
        self.expect_answer("report status", answer.report_status.as_deref(), "pending");
        let Some(ledger) = ledger else {
            return;
        };
        self.expect_ledger_state(ledger, "paused");
        let broken = ledger_node(ledger, "review-broken");
        self.expect_node(
            broken,
            "status",
            "error",
            "ledger review-broken node is not error",
        );
        if node_str(broken, "error").is_none_or(str::is_empty) {
            self.problems
                .push("ledger review-broken node has no error message".to_string());
        }
        self.expect_node(
            ledger_node(ledger, "report"),
            "status",
            "pending",
            "ledger report node is not pending",
        );
        if has_event(ledger, "spawned", "node", "report") {
            self.problems
                .push("report node started despite the escalation pause".to_string());
        }
        if !has_event(ledger, "milestone", "milestone", "paused") {
            self.problems
                .push("ledger has no paused milestone".to_string());
        }
    }

    /// The broken-spec probe: the run call is rejected before any child starts.
    fn dry_run_reject(&mut self) {
        let answer = self.answer;
        if answer.rejected != Some(true) {
            self.problems
                .push("ANSWER does not report the run call as rejected".to_string());
        }
        if answer.children != Some(0) {
            self.problems.push(format!(
                "ANSWER children count is {}, expected 0",
                answer
                    .children
                    .map_or_else(|| "unset".to_string(), |children| children.to_string())
            ));
        }
        if !answer
            .message
            .as_deref()
            .is_some_and(|message| message.contains("no-such-subagent-entry"))
        {
            self.problems
                .push("ANSWER message does not mention the unknown subagent reference".to_string());
        }
    }
}

/// The outcome of one [`check_task_success`] call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCheckOutcome {
    pub ok: bool,
    pub problems: Vec<String>,
}
