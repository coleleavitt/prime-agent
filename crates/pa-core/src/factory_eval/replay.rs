//! The deterministic replay check over saved factory status ledgers: stable event
//! identities and complete resource accounting.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The event-kind vocabulary the replay checker accepts (the executor's
/// ledger kinds, machine form included).
pub const KNOWN_FACTORY_EVENT_KINDS: &[&str] = &[
    "run_started",
    "node_ready",
    "state_entry",
    "transition_fired",
    "transition_blocked",
    "wait_settled",
    "spawned",
    "spawn_backoff",
    "spawn_deferred",
    "settled",
    "answer_captured",
    "retry",
    "node_error",
    "node_cancelled",
    "cancelled",
    "cancel_failed",
    "milestone",
    "run_stopped",
    "resumed",
    "executor_error",
    // A host restart found the run in flight (the durable executor's
    // recovery: one per run, one per lost in-flight instance).
    "run_interrupted",
    "interrupted",
];

const KNOWN_STAGES: &[&str] = &["recorded", "arrived", "shown", "delivered"];
const LEDGER_STATES: &[&str] = &["running", "stopping", "paused", "done", "failed", "stopped"];
const INSTANCE_ID_PATTERN: fn(&str) -> bool = |id: &str| {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
};

/// The result of one replay check: the problems it found (empty = ok).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LedgerCheckResult {
    pub ok: bool,
    pub problems: Vec<String>,
}

fn is_ledger_shape(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    let run_id = obj.get("run_id").and_then(Value::as_str);
    let spec_id = obj.get("spec_id").and_then(Value::as_str);
    let state = obj.get("state").and_then(Value::as_str);
    let nodes = obj.get("nodes").and_then(Value::as_array);
    let events = obj.get("events").and_then(Value::as_array);
    let elapsed = obj.get("elapsed_ms").and_then(Value::as_i64);
    let usage = obj.get("usage");
    run_id.is_some_and(|id| !id.is_empty())
        && spec_id.is_some_and(|id| !id.is_empty())
        && state.is_some_and(|state| LEDGER_STATES.contains(&state))
        && nodes.is_some_and(|nodes| !nodes.is_empty())
        && events.is_some()
        && elapsed.is_some_and(|elapsed| elapsed >= 0)
        && usage.is_some_and(Value::is_object)
}

/// Deterministic replay check over one saved status ledger: stable event
/// identities (contiguous seq, closed kind/stage vocabularies, valid node
/// refs) and complete resource accounting (every spawned instance settled
/// with a duration, cancelled, or still in flight on a paused run; usage
/// counts match the event stream). Machine-form ledgers add the
/// `state_entry` contiguity, transition endpoint, wait-settled, and
/// `transitions_fired` rules.
///
/// # Panics
///
/// Panics only on internal shape violations that the leading shape check
/// already ruled out (the `expect` calls sit behind `is_ledger_shape`).
#[must_use]
pub fn check_replay_ledger(ledger: &Value) -> LedgerCheckResult {
    if !is_ledger_shape(ledger) {
        return LedgerCheckResult {
            ok: false,
            problems: vec!["ledger does not match the factory status shape".to_string()],
        };
    }
    let mut replay = LedgerReplay::new(
        ledger["nodes"].as_array().expect("checked shape"),
        ledger["state"].as_str().expect("checked shape"),
    );
    let tally = replay.tally_events(ledger["events"].as_array().expect("checked shape"));
    if tally.truncated {
        replay.problems.push(
            "event window is truncated (first seq is not 1); count assertions skipped".to_string(),
        );
    } else {
        replay.check_state_entries(&tally);
        replay.check_instance_statuses(&tally);
        replay.check_spawn_settle_pairing(&tally);
        replay.check_usage_counts(&tally, &ledger["usage"]);
        replay.check_milestones(&tally);
    }
    LedgerCheckResult {
        ok: replay.problems.is_empty(),
        problems: replay.problems,
    }
}

/// What one pass over a ledger's events recorded; instance keys are `<node>#<instance>`.
#[derive(Default)]
struct EventTally {
    /// The window does not start at seq 1: the count assertions do not apply.
    truncated: bool,
    spawned: BTreeMap<String, i64>,
    spawned_per_node: BTreeMap<String, i64>,
    settled: BTreeMap<String, i64>,
    cancelled: BTreeSet<String>,
    state_entries: BTreeMap<String, Vec<i64>>,
    wait_settled_nodes: BTreeSet<String>,
    milestones: Vec<String>,
    run_stopped: bool,
    transitions_fired: i64,
}

/// One ledger under the replay check: its nodes, its run state, and the problems found.
struct LedgerReplay<'a> {
    nodes: &'a [Value],
    state: &'a str,
    node_ids: Vec<String>,
    problems: Vec<String>,
}

impl<'a> LedgerReplay<'a> {
    /// Collect the node ids, reporting invalid and duplicate ones.
    fn new(nodes: &'a [Value], state: &'a str) -> Self {
        let mut problems = Vec::new();
        let mut node_ids: Vec<String> = Vec::new();
        for node in nodes {
            let Some(id) = node.get("id").and_then(Value::as_str) else {
                problems.push("ledger node has an invalid id: null".to_string());
                continue;
            };
            if !INSTANCE_ID_PATTERN(id) {
                problems.push(format!("ledger node has an invalid id: {id:?}"));
            } else if node_ids.iter().any(|known| known == id) {
                problems.push(format!("ledger duplicates node id {id}"));
            }
            node_ids.push(id.to_string());
        }
        Self {
            nodes,
            state,
            node_ids,
            problems,
        }
    }

    fn node_exists(&self, id: &str) -> bool {
        self.node_ids.iter().any(|known| known == id)
    }

    /// Whether the node instance `key` (`<node>#<index>`) reports one of `statuses`.
    fn instance_has_status(&self, key: &str, statuses: &[&str]) -> bool {
        let node_id = key.split('#').next().unwrap_or_default();
        self.nodes.iter().any(|node| {
            node.get("id").and_then(Value::as_str) == Some(node_id)
                && node
                    .get("instances")
                    .and_then(Value::as_array)
                    .is_some_and(|instances| {
                        instances.iter().any(|instance| {
                            let index = instance.get("index").and_then(Value::as_i64).unwrap_or(-1);
                            format!("{node_id}#{index}") == key
                                && instance
                                    .get("status")
                                    .and_then(Value::as_str)
                                    .is_some_and(|status| statuses.contains(&status))
                        })
                    })
        })
    }

    /// The single pass over the event stream: per-event identity checks and the counts the
    /// later checks compare.
    fn tally_events(&mut self, events: &[Value]) -> EventTally {
        let mut tally = EventTally::default();
        let mut last_seq: i64 = 0;
        for (index, event) in events.iter().enumerate() {
            let Some(event) = event.as_object() else {
                self.problems
                    .push(format!("events[{index}] is not an object"));
                continue;
            };
            let Some(seq) = self.accept_seq(index, event, last_seq) else {
                continue;
            };
            if index == 0 && seq != 1 {
                tally.truncated = true;
            }
            last_seq = seq;
            let kind = event
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            self.check_event_identity(index, event, kind);
            self.record_event(index, event, kind, &mut tally);
        }
        tally
    }

    /// The event's `seq`, when it increases past `last_seq` (a gap is reported but kept).
    fn accept_seq(
        &mut self,
        index: usize,
        event: &Map<String, Value>,
        last_seq: i64,
    ) -> Option<i64> {
        let Some(seq) = event.get("seq").and_then(Value::as_i64) else {
            self.problems
                .push(format!("events[{index}] has a non-increasing seq: null"));
            return None;
        };
        if seq <= last_seq {
            self.problems
                .push(format!("events[{index}] has a non-increasing seq: {seq}"));
            return None;
        }
        if seq != last_seq + 1 {
            self.problems.push(format!(
                "events[{index}] has a seq gap: expected {}, got {seq} (dropped event)",
                last_seq + 1
            ));
        }
        Some(seq)
    }

    /// The closed kind and stage vocabularies, and the node and transition endpoints the
    /// event names.
    fn check_event_identity(&mut self, index: usize, event: &Map<String, Value>, kind: &str) {
        if !KNOWN_FACTORY_EVENT_KINDS.contains(&kind) {
            self.problems
                .push(format!("events[{index}] has an unknown kind: {kind:?}"));
        }
        if let Some(stage) = event
            .get("stage")
            .and_then(Value::as_str)
            .filter(|stage| !KNOWN_STAGES.contains(stage))
        {
            self.problems
                .push(format!("events[{index}] has an unknown stage: {stage:?}"));
        }
        if let Some(node) = event
            .get("node")
            .and_then(Value::as_str)
            .filter(|node| !self.node_exists(node))
        {
            self.problems
                .push(format!("events[{index}] references unknown node {node:?}"));
        }
        if !matches!(kind, "transition_fired" | "transition_blocked") {
            return;
        }
        // The executor always emits both endpoints; absence is drift.
        let (Some(from), Some(to)) = (
            event.get("from").and_then(Value::as_str),
            event.get("to").and_then(Value::as_str),
        ) else {
            self.problems.push(format!(
                "events[{index}] {kind} requires from and to states"
            ));
            return;
        };
        if !self.node_exists(from) {
            self.problems.push(format!(
                "events[{index}] transitions from unknown state {from:?}"
            ));
        }
        if !self.node_exists(to) {
            self.problems.push(format!(
                "events[{index}] transitions to unknown state {to:?}"
            ));
        }
    }

    /// Count the event into the tally, checking the fields its kind requires.
    fn record_event(
        &mut self,
        index: usize,
        event: &Map<String, Value>,
        kind: &str,
        tally: &mut EventTally,
    ) {
        let node = event.get("node").and_then(Value::as_str);
        let instance = event.get("instance").and_then(Value::as_i64).unwrap_or(-1);
        let key = node.map(|node| format!("{node}#{instance}"));
        match kind {
            "state_entry" => {
                let entry = event.get("entry").and_then(Value::as_i64);
                match (node.filter(|node| self.node_exists(node)), entry) {
                    (Some(node), Some(entry)) if entry >= 0 => {
                        tally
                            .state_entries
                            .entry(node.to_string())
                            .or_default()
                            .push(entry);
                    }
                    (Some(_), _) => self.problems.push(format!(
                        "events[{index}] state_entry requires a non-negative integer entry index"
                    )),
                    (None, _) => self.problems.push(format!(
                        "events[{index}] state_entry references unknown node {}",
                        node.map_or_else(|| "null".to_string(), |node| format!("{node:?}"))
                    )),
                }
            }
            "wait_settled" => {
                if let Some(node) = node {
                    tally.wait_settled_nodes.insert(node.to_string());
                }
            }
            "transition_fired" => tally.transitions_fired += 1,
            "spawned" => {
                if let Some(key) = key {
                    *tally.spawned.entry(key).or_insert(0) += 1;
                }
                if let Some(node) = node {
                    *tally.spawned_per_node.entry(node.to_string()).or_insert(0) += 1;
                }
            }
            "settled" => {
                if let Some(key) = key {
                    *tally.settled.entry(key).or_insert(0) += 1;
                }
                self.check_settle_fields(index, event);
            }
            "cancelled" => {
                if let Some(key) = key {
                    tally.cancelled.insert(key);
                }
            }
            "milestone" => {
                if let Some(milestone) = event.get("milestone").and_then(Value::as_str) {
                    tally.milestones.push(milestone.to_string());
                }
            }
            "run_stopped" => tally.run_stopped = true,
            _ => {}
        }
    }

    /// A `settled` event: `done` carries a duration, `error` an error message, and no other
    /// status is valid.
    fn check_settle_fields(&mut self, index: usize, event: &Map<String, Value>) {
        let status = event.get("status").and_then(Value::as_str);
        if status == Some("done")
            && event
                .get("duration_ms")
                .and_then(Value::as_i64)
                .is_none_or(|duration| duration < 0)
        {
            self.problems.push(format!(
                "events[{index}] settles done without a duration_ms"
            ));
        }
        if status == Some("error") && event.get("error").and_then(Value::as_str).is_none() {
            self.problems.push(format!(
                "events[{index}] settles error without an error message"
            ));
        }
        if let Some(status) = status.filter(|status| !matches!(*status, "done" | "error")) {
            self.problems.push(format!(
                "events[{index}] has an invalid settled status: {status:?}"
            ));
        }
    }

    /// Wait states never spawn, and each node's `state_entry` indices are contiguous
    /// `0..n-1` and match its `entries` report (a gap means a dropped or forged event).
    fn check_state_entries(&mut self, tally: &EventTally) {
        for node_id in &tally.wait_settled_nodes {
            let spawned = tally.spawned_per_node.get(node_id).copied().unwrap_or(0);
            if spawned > 0 {
                self.problems.push(format!(
                    "node {node_id} settled a wait but spawned {spawned} instance(s)"
                ));
            }
        }
        for node in self.nodes {
            let Some(id) = node.get("id").and_then(Value::as_str) else {
                continue;
            };
            let mut indices = tally.state_entries.get(id).cloned().unwrap_or_default();
            indices.sort_unstable();
            let contiguous = indices
                .iter()
                .enumerate()
                .all(|(position, entry)| *entry == position as i64);
            if !contiguous {
                self.problems.push(format!(
                    "node {id} state_entry indices are not contiguous 0..n-1: {indices:?}"
                ));
            }
            if let Some(entries) = node.get("entries").and_then(Value::as_array) {
                let mut reported: Vec<i64> = entries
                    .iter()
                    .filter_map(|entry| entry.get("index").and_then(Value::as_i64))
                    .collect();
                reported.sort_unstable();
                if reported != indices {
                    self.problems.push(format!(
                        "node {id} entries {reported:?} do not match its state_entry events {indices:?}"
                    ));
                }
            }
        }
    }

    /// Whether instances may still be pending or running (the run has not ended).
    fn in_flight_ok(&self) -> bool {
        matches!(self.state, "paused" | "running" | "stopping")
    }

    /// Complete resource accounting per reported instance: its status has the events that
    /// settle it.
    fn check_instance_statuses(&mut self, tally: &EventTally) {
        let in_flight_ok = self.in_flight_ok();
        let state = self.state;
        for node in self.nodes {
            let Some(id) = node.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(instances) = node.get("instances").and_then(Value::as_array) else {
                continue;
            };
            for instance in instances {
                let index = instance.get("index").and_then(Value::as_i64).unwrap_or(-1);
                let key = format!("{id}#{index}");
                let has_spawn = tally.spawned.get(&key).copied().unwrap_or(0) > 0;
                let settle_count = tally.settled.get(&key).copied().unwrap_or(0);
                let status = instance
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match status {
                    "done" if settle_count == 0 => self.problems.push(format!(
                        "node {id} instance {index} is done without a settle event"
                    )),
                    "done" => {
                        let duration = instance.get("duration_ms").and_then(Value::as_i64);
                        if duration.is_none_or(|duration| duration < 0) {
                            self.problems.push(format!(
                                "node {id} instance {index} is done without a duration_ms"
                            ));
                        }
                    }
                    "error" => {
                        let error = instance.get("error").and_then(Value::as_str);
                        if settle_count == 0 {
                            self.problems.push(format!(
                                "node {id} instance {index} errored without a settle event"
                            ));
                        } else if error.is_none_or(str::is_empty) {
                            self.problems.push(format!(
                                "node {id} instance {index} errored without an error message"
                            ));
                        }
                        if has_spawn && instance.get("duration_ms").is_none() {
                            self.problems.push(format!(
                                "node {id} instance {index} errored after spawn without a duration_ms"
                            ));
                        }
                    }
                    "cancelled" if !tally.cancelled.contains(&key) => {
                        self.problems.push(format!(
                            "node {id} instance {index} is cancelled without a cancel event"
                        ));
                    }
                    "running" | "pending" if !in_flight_ok => {
                        self.problems.push(format!(
                            "node {id} instance {index} is {status} in a {state} ledger"
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    /// Every spawn settles or is cancelled (or is still in flight on a live run), and every
    /// settle follows a spawn except an admission failure's.
    fn check_spawn_settle_pairing(&mut self, tally: &EventTally) {
        let in_flight_ok = self.in_flight_ok();
        for (key, count) in &tally.spawned {
            let settled_count = tally.settled.get(key).copied().unwrap_or(0);
            if settled_count == 0
                && !tally.cancelled.contains(key)
                && !(in_flight_ok && self.instance_has_status(key, &["pending", "running"]))
            {
                self.problems.push(format!(
                    "spawned instance {key} never settled or cancelled ({count} spawn event(s))"
                ));
            }
        }
        for (key, count) in &tally.settled {
            if tally.spawned.get(key).copied().unwrap_or(0) == 0
                && *count > 0
                && !self.instance_has_status(key, &["error"])
            {
                self.problems
                    .push(format!("instance {key} settled without ever being spawned"));
            }
        }
    }

    /// The ledger's `usage` counters match the event stream.
    fn check_usage_counts(&mut self, tally: &EventTally, usage: &Value) {
        let spawn_events: i64 = tally.spawned.values().sum();
        // Count settled EVENTS on spawned keys, not distinct keys: a
        // retried instance settles twice on the same key and the executor's
        // settle_count increments per settlement (including retries).
        let collect_settles: i64 = tally
            .settled
            .iter()
            .filter(|(key, _)| tally.spawned.get(*key).copied().unwrap_or(0) > 0)
            .map(|(_, count)| *count)
            .sum();
        let usage_spawns = usage["spawns"].as_i64().unwrap_or(-1);
        if usage_spawns != spawn_events {
            self.problems.push(format!(
                "usage.spawns {usage_spawns} does not match {spawn_events} spawned event(s)"
            ));
        }
        let usage_settled = usage["settled"].as_i64().unwrap_or(-1);
        if usage_settled != collect_settles {
            self.problems.push(format!(
                "usage.settled {usage_settled} does not match {collect_settles} collect settlement(s)"
            ));
        }
        if let Some(usage_transitions) = usage["transitions_fired"]
            .as_i64()
            .filter(|transitions| *transitions != tally.transitions_fired)
        {
            self.problems.push(format!(
                "usage.transitions_fired {usage_transitions} does not match {} transition_fired event(s)",
                tally.transitions_fired
            ));
        }
    }

    /// The run state's milestone identities.
    fn check_milestones(&mut self, tally: &EventTally) {
        let reached = |names: &[&str]| {
            tally
                .milestones
                .iter()
                .any(|milestone| names.contains(&milestone.as_str()))
        };
        let missing = match self.state {
            "done" if !reached(&["finished"]) => "run state done without a finished milestone",
            "failed" if !reached(&["failed"]) => "run state failed without a failed milestone",
            "paused" if !reached(&["paused", "budget_exceeded", "max_transitions_exceeded"]) => {
                "run state paused without a paused, budget_exceeded, or max_transitions_exceeded milestone"
            }
            "stopped" if !tally.run_stopped => "run state stopped without a run_stopped event",
            _ => return,
        };
        self.problems.push(missing.to_string());
    }
}

/// Run the replay check over a saved `report.json` or a single status
/// ledger. A report written by [`serialize_eval_report`] carries its
/// trials under `results`; the older hand-built `trials` key is still
/// accepted.
#[must_use]
pub fn run_replay_checks(data: &Value) -> ReplayOutcome {
    let mut ledgers: Vec<ReplayLedgerOutcome> = Vec::new();
    let trials = data
        .get("results")
        .and_then(Value::as_array)
        .or_else(|| data.get("trials").and_then(Value::as_array));
    if let Some(trials) = trials {
        for trial in trials {
            let Some(trial) = trial.as_object() else {
                continue;
            };
            let Some(ledger) = trial.get("ledger") else {
                continue;
            };
            if ledger.is_null() {
                continue;
            }
            let id = format!(
                "{}/{}/trial-{}",
                trial.get("factory").and_then(Value::as_str).unwrap_or("?"),
                trial.get("arm").and_then(Value::as_str).unwrap_or("?"),
                trial.get("trial").and_then(Value::as_i64).unwrap_or(-1)
            );
            let result = check_replay_ledger(ledger);
            ledgers.push(ReplayLedgerOutcome { id, result });
        }
    } else {
        let result = check_replay_ledger(data);
        ledgers.push(ReplayLedgerOutcome {
            id: "ledger".to_string(),
            result,
        });
    }
    let ok = !ledgers.is_empty() && ledgers.iter().all(|entry| entry.result.ok);
    ReplayOutcome { ok, ledgers }
}

/// The outcome of [`run_replay_checks`]: one entry per checked ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayLedgerOutcome {
    pub id: String,
    pub result: LedgerCheckResult,
}

/// The folded replay outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayOutcome {
    pub ok: bool,
    pub ledgers: Vec<ReplayLedgerOutcome>,
}
