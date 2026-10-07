//! The executor's run model: one run, its states, their entries
//! (activations), and each entry's spawned instances.
//!
//! The whole model is serializable: the store persists a run record after
//! every committed mutation, and a restarted host rebuilds the registry
//! from those records (see [`super::store`]).

use std::collections::{BTreeSet, HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One instance's or entry's lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Running,
    Done,
    Error,
    Cancelled,
}

impl Status {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }

    /// `pending | running`: the entry or instance is still in flight.
    #[must_use]
    pub fn in_flight(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }

    /// `done | error | cancelled` (`TERMINAL_ENTRY_STATUSES`).
    #[must_use]
    pub fn terminal(self) -> bool {
        !self.in_flight()
    }
}

/// A run's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Stopping,
    Paused,
    Done,
    Failed,
    Stopped,
}

impl RunState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    /// `running | stopping | paused`: the run itself is live.
    #[must_use]
    pub fn live(self) -> bool {
        matches!(self, Self::Running | Self::Stopping | Self::Paused)
    }
}

/// One spawned child of one state entry (a foreach entry has one per item).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeInstance {
    /// Per-state running counter; unique within the state.
    pub index: u64,
    /// Fully rendered; re-spawns reuse it verbatim.
    pub prompt: String,
    pub status: Status,
    /// Spawn admissions tried (a rate-limit deferral counts too, and
    /// `retries` compares against this same counter).
    pub attempt: u32,
    /// Consecutive rate-limited admissions on the control loop's backoff
    /// path; a successful spawn resets it.
    pub rate_limit_streak: u32,
    pub child_id: Option<String>,
    /// Admission time on the executor clock (seconds).
    pub spawned_at: Option<f64>,
    pub duration_ms: Option<u64>,
    /// The capped collect preview.
    pub answer: Option<String>,
    pub error: Option<String>,
    pub tool_uses: u64,
    /// The child a host restart lost track of while this instance was in
    /// flight; resume deletes it best-effort before re-admitting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_child: Option<String>,
}

impl NodeInstance {
    #[must_use]
    pub fn new(index: u64, prompt: String) -> Self {
        Self {
            index,
            prompt,
            status: Status::Pending,
            attempt: 0,
            rate_limit_streak: 0,
            child_id: None,
            spawned_at: None,
            duration_ms: None,
            answer: None,
            error: None,
            tool_uses: 0,
            interrupted_child: None,
        }
    }
}

/// One entry (activation) of a state; re-entry creates a fresh entry. An
/// entry settles when all of its instances settle done; the settle captures
/// the state's declared outputs, and the control loop evaluates the
/// outgoing transitions once (`consumed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateEntry {
    pub index: u64,
    pub status: Status,
    pub instances: Vec<NodeInstance>,
    pub error: Option<String>,
    /// The joined captured answers of this entry.
    pub answer: Option<String>,
    /// Captured settle outputs (port name -> value).
    pub outputs: Option<Map<String, Value>>,
    /// Ports whose json capture failed (port name -> error sentence).
    pub output_errors: Option<Map<String, Value>>,
    pub is_settle: bool,
    pub consumed: bool,
}

impl StateEntry {
    #[must_use]
    pub fn new(index: u64) -> Self {
        Self {
            index,
            status: Status::Pending,
            instances: Vec::new(),
            error: None,
            answer: None,
            outputs: None,
            output_errors: None,
            is_settle: false,
            consumed: false,
        }
    }
}

/// Executor-side state for one machine state of one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateRun {
    pub state_id: String,
    /// The canonical state spec.
    pub spec: Value,
    pub prompt_template: Option<String>,
    /// The configured inline subagent name; labels children.
    pub name: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub max_entries: u64,
    pub entries_used: u64,
    pub entries: Vec<StateEntry>,
    pub instance_counter: u64,
    pub error: Option<String>,
    /// Set by `stop()` and `fail_fast` for never-entered states.
    pub cancelled: bool,
}

impl StateRun {
    #[must_use]
    pub fn lifecycle(&self) -> &str {
        self.spec
            .get("lifecycle")
            .and_then(Value::as_str)
            .unwrap_or(crate::factory::spec::NODE_LIFECYCLE_DEFAULT)
    }

    #[must_use]
    pub fn resident(&self) -> bool {
        self.lifecycle() == "resident"
    }

    /// The latest entry's status; `cancelled`/`pending` before any entry.
    #[must_use]
    pub fn status(&self) -> Status {
        match self.entries.last() {
            Some(entry) => entry.status,
            None if self.cancelled => Status::Cancelled,
            None => Status::Pending,
        }
    }

    /// The newest entry that settled (done or error).
    #[must_use]
    pub fn latest_settle(&self) -> Option<&StateEntry> {
        self.entries.iter().rev().find(|entry| entry.is_settle)
    }

    /// The canonical spec's integer field `key` (`spec.get(key, default)`).
    #[must_use]
    pub fn spec_u64(&self, key: &str, default: u64) -> u64 {
        self.spec
            .get(key)
            .and_then(Value::as_u64)
            .unwrap_or(default)
    }

    /// The failure policy (`spec.get("failure_policy", "escalate")`).
    #[must_use]
    pub fn failure_policy(&self) -> &str {
        self.spec
            .get("failure_policy")
            .and_then(Value::as_str)
            .unwrap_or(crate::factory::spec::RUN_FAILURE_POLICY_DEFAULT)
    }
}

/// One queued settle evaluation: the state, the entry, and the transition
/// index to resume from (0 for a fresh settle, the paused index after a
/// `max_transitions` pause).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingEvaluation {
    pub state_id: String,
    pub entry: usize,
    pub resume_from: usize,
}

/// The last-fired source-settle signature of one join transition, keyed by
/// the transition's position (duplicate joins stay independent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinMark {
    pub transition: usize,
    pub to: String,
    /// `(source state, settle entry index)`, sorted.
    pub signature: Vec<(String, u64)>,
}

/// Executor-side state for one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactoryRun {
    pub run_id: String,
    pub spec_id: String,
    pub name: Option<String>,
    /// The canonical machine this run executes (read-only).
    pub machine: Value,
    pub state: RunState,
    /// Run start on the executor clock (seconds).
    pub started_at: f64,
    pub max_parallel: u64,
    pub max_transitions: u64,
    pub max_children: u64,
    pub max_transitions_reported: bool,
    pub max_children_reported: bool,
    pub run_budget_ms: Option<u64>,
    pub budget_reported: bool,
    pub pause_reason: Option<String>,
    /// States in declared order (the deterministic iteration order).
    pub states: Vec<StateRun>,
    pub pending_evaluations: VecDeque<PendingEvaluation>,
    /// The ledger: one JSON object per event, in `seq` order.
    pub events: Vec<Value>,
    pub milestones: BTreeSet<String>,
    pub spawn_count: u64,
    pub settle_count: u64,
    pub transitions_fired: u64,
    pub tool_use_total: u64,
    /// The control loop's generation: `resume()` bumps it so a winding-down
    /// pause-path loop can never continue as a second control loop.
    pub loop_generation: u64,
    /// The rate-limit backoff deadline on the executor clock.
    pub admission_backoff_until: Option<f64>,
    pub join_fired: Vec<JoinMark>,
    /// Bumped on every observable mutation (watch and persistence).
    pub revision: u64,
    /// The library machine a run came from (`machine` / `machine_path`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library: Option<LibraryOrigin>,
    /// Derived from `machine` (not persisted): transition indices per
    /// source state; a join is visible from every source.
    #[serde(skip)]
    pub transitions_from: HashMap<String, Vec<usize>>,
    /// Derived (not persisted): state id -> position in `states`.
    #[serde(skip)]
    pub position_of: HashMap<String, usize>,
}

/// Where a library run's machine came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryOrigin {
    pub name: String,
    pub path: Option<String>,
}

/// The ledger's event kinds the executor emits (the factory eval's replay
/// vocabulary).
pub mod kind {
    pub const RUN_STARTED: &str = "run_started";
    pub const STATE_ENTRY: &str = "state_entry";
    pub const NODE_READY: &str = "node_ready";
    pub const SPAWNED: &str = "spawned";
    pub const SPAWN_BACKOFF: &str = "spawn_backoff";
    pub const SPAWN_DEFERRED: &str = "spawn_deferred";
    pub const SETTLED: &str = "settled";
    pub const ANSWER_CAPTURED: &str = "answer_captured";
    pub const RETRY: &str = "retry";
    pub const NODE_ERROR: &str = "node_error";
    pub const NODE_CANCELLED: &str = "node_cancelled";
    pub const CANCELLED: &str = "cancelled";
    pub const CANCEL_FAILED: &str = "cancel_failed";
    pub const TRANSITION_FIRED: &str = "transition_fired";
    pub const TRANSITION_BLOCKED: &str = "transition_blocked";
    pub const MILESTONE: &str = "milestone";
    pub const RESUMED: &str = "resumed";
    pub const RUN_STOPPED: &str = "run_stopped";
    pub const EXECUTOR_ERROR: &str = "executor_error";
    /// A host restart found the run in flight (one per run).
    pub const RUN_INTERRUPTED: &str = "run_interrupted";
    /// A host restart lost an in-flight instance's child (one per instance).
    pub const INTERRUPTED: &str = "interrupted";
}

/// The optional addressing fields of one ledger event.
#[derive(Debug, Default, Clone, Copy)]
pub struct EventAt<'a> {
    pub node: Option<&'a str>,
    pub entry: Option<u64>,
    pub instance: Option<u64>,
    pub detail: Option<&'a str>,
}

impl EventAt<'_> {
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }
}

impl FactoryRun {
    /// Rebuild the derived indices from `machine` and `states`.
    pub fn index(&mut self) {
        self.position_of = self
            .states
            .iter()
            .enumerate()
            .map(|(position, state)| (state.state_id.clone(), position))
            .collect();
        self.transitions_from.clear();
        if let Some(transitions) = self.machine.get("transitions").and_then(Value::as_array) {
            for (index, transition) in transitions.iter().enumerate() {
                match transition.get("from") {
                    Some(Value::Array(sources)) => {
                        for source in sources.iter().filter_map(Value::as_str) {
                            self.transitions_from
                                .entry(source.to_string())
                                .or_default()
                                .push(index);
                        }
                    }
                    Some(Value::String(source)) => {
                        self.transitions_from
                            .entry(source.clone())
                            .or_default()
                            .push(index);
                    }
                    _ => {}
                }
            }
        }
    }

    /// The machine's transition at `index`.
    #[must_use]
    pub fn transition(&self, index: usize) -> &Value {
        &self.machine["transitions"][index]
    }

    #[must_use]
    pub fn position(&self, state_id: &str) -> Option<usize> {
        self.position_of.get(state_id).copied()
    }

    /// Append one ledger event. Stages follow the spec: `arrived` (a child
    /// answer settled and was captured), `recorded` (everything else),
    /// `shown` (a milestone notice reached the parent), `delivered` (the
    /// parent read the ledger via `status()`). Returns the event's `seq`.
    pub fn event(
        &mut self,
        event_kind: &str,
        at: EventAt<'_>,
        stage: &str,
        extra: Vec<(&str, Value)>,
    ) -> u64 {
        let seq = self.events.len() as u64 + 1;
        let mut event = Map::new();
        event.insert("seq".into(), Value::from(seq));
        event.insert("kind".into(), Value::from(event_kind));
        event.insert("stage".into(), Value::from(stage));
        if let Some(node) = at.node {
            event.insert("node".into(), Value::from(node));
        }
        if let Some(entry) = at.entry {
            event.insert("entry".into(), Value::from(entry));
        }
        if let Some(instance) = at.instance {
            event.insert("instance".into(), Value::from(instance));
        }
        if let Some(detail) = at.detail {
            event.insert("detail".into(), Value::from(detail));
        }
        for (key, value) in extra {
            event.insert(key.to_string(), value);
        }
        self.events.push(Value::Object(event));
        self.touch();
        seq
    }

    /// A `recorded` event.
    pub fn record(&mut self, event_kind: &str, at: EventAt<'_>, extra: Vec<(&str, Value)>) -> u64 {
        self.event(event_kind, at, "recorded", extra)
    }

    /// Bump the watch revision (every observable mutation).
    pub fn touch(&mut self) {
        self.revision += 1;
    }

    /// Instances currently admitted and in flight.
    #[must_use]
    pub fn running_instance_count(&self) -> usize {
        self.instances()
            .filter(|(_, _, instance)| instance.status == Status::Running)
            .count()
    }

    /// Every `(state, entry, instance)` in deterministic order.
    pub fn instances(&self) -> impl Iterator<Item = (&StateRun, &StateEntry, &NodeInstance)> {
        self.states.iter().flat_map(|state| {
            state.entries.iter().flat_map(move |entry| {
                entry
                    .instances
                    .iter()
                    .map(move |instance| (state, entry, instance))
            })
        })
    }

    #[must_use]
    pub fn has_pending_instance(&self) -> bool {
        self.instances()
            .any(|(_, _, instance)| instance.status == Status::Pending)
    }

    /// The state ids whose latest status is `pending`.
    #[must_use]
    pub fn pending_state_ids(&self) -> Vec<String> {
        self.states
            .iter()
            .filter(|state| state.status() == Status::Pending)
            .map(|state| state.state_id.clone())
            .collect()
    }
}
