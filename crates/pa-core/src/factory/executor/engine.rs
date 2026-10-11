//! The state machine itself: entries, transitions, binding, admission,
//! settlement, failure policies, completion, and the control loop.
//!
//! Concurrency model: every seam call (spawn, collect, delete, notice,
//! sleep) is an await point at which other callers (`stop()`, `resume()`,
//! the activity lane) may mutate the run, exactly like the original
//! single-threaded asyncio executor. The run lock is held only between
//! awaits ([`RunCell::with`]), and every continuation re-checks the run's
//! state and loop generation before acting on what it saw.

use std::fmt::Write as _;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::binding::{
    ANSWER_BINDING_CAP,
    ANSWER_CAPTURE_CAP,
    char_prefix,
    guard_passes,
    is_rate_limit_error,
    json_repr,
    parse_json_output,
    py_json_dumps,
    render_prompt,
    text_of,
};
use super::model::{
    EventAt,
    FactoryRun,
    JoinMark,
    NodeInstance,
    PendingEvaluation,
    RunState,
    StateEntry,
    Status,
    kind,
};
use super::ports::FactorySpawn;
use super::{Inner, RunCell};
use crate::factory::labels::spawn_label;
use crate::factory::pyvalue::py_str_repr;
use crate::factory::spec::NODE_RETRIES_DEFAULT;
use crate::session_engine::rlm_host::RlmChildResult;

/// Spawn admissions per instance before a persistent rate limit fails it.
pub const BACKOFF_MAX_ATTEMPTS: u32 = 5;
pub const BACKOFF_BASE_SECONDS: f64 = 1.0;
pub const BACKOFF_CAP_SECONDS: f64 = 60.0;
/// How long each control-loop collect waits for unsettled children.
pub const POLL_TIMEOUT_MS: u64 = 2000;

/// One instance's address: state position, entry index, instance position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Slot {
    pub state: usize,
    pub entry: usize,
    pub instance: usize,
}

/// What one admission attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Admitted,
    Deferred,
    Failed,
    Stopped,
}

/// The control loop's failure: the Python executor reported a crashed
/// loop as `"<ExceptionType>: <message>"`; host failures were
/// `RuntimeError`s.
#[derive(Debug)]
struct LoopError(String);

/// `{delay:g}` for the backoff detail (whole seconds print bare).
fn seconds_text(seconds: f64) -> String {
    if seconds.fract() == 0.0 && seconds.abs() < 1e15 {
        format!("{}", seconds as i64)
    } else {
        format!("{seconds}")
    }
}

impl FactoryRun {
    fn slot_instance(&mut self, slot: Slot) -> &mut NodeInstance {
        &mut self.states[slot.state].entries[slot.entry].instances[slot.instance]
    }

    fn slot_at(&self, slot: Slot) -> EventAt<'_> {
        let state = &self.states[slot.state];
        EventAt {
            node: Some(&state.state_id),
            entry: Some(state.entries[slot.entry].index),
            instance: Some(state.entries[slot.entry].instances[slot.instance].index),
            detail: None,
        }
    }

    /// Record one event addressed at an instance slot.
    fn record_at(
        &mut self,
        event_kind: &str,
        slot: Slot,
        detail: Option<&str>,
        extra: Vec<(&str, Value)>,
    ) {
        let state = &self.states[slot.state];
        let node = state.state_id.clone();
        let entry = state.entries[slot.entry].index;
        let instance = state.entries[slot.entry].instances[slot.instance].index;
        self.record(
            event_kind,
            EventAt {
                node: Some(&node),
                entry: Some(entry),
                instance: Some(instance),
                detail,
            },
            extra,
        );
    }

    /// Create one new entry of a state (bounded by `max_entries` upstream).
    pub(super) fn enter_state(&mut self, position: usize, from_state: Option<&str>) {
        let state = &mut self.states[position];
        let index = state.entries.len() as u64;
        state.entries.push(StateEntry::new(index));
        state.entries_used += 1;
        let node = state.state_id.clone();
        let detail = from_state.map_or_else(
            || "entry state".to_string(),
            |from| format!("entered from {from}"),
        );
        self.record(
            kind::STATE_ENTRY,
            EventAt {
                node: Some(&node),
                entry: Some(index),
                instance: None,
                detail: Some(&detail),
            },
            Vec::new(),
        );
    }

    fn queue_settle(&mut self, position: usize, entry: usize) {
        let state = &mut self.states[position];
        state.entries[entry].is_settle = true;
        let state_id = state.state_id.clone();
        self.pending_evaluations.push_back(PendingEvaluation {
            state_id,
            entry,
            resume_from: 0,
        });
    }

    /// Quiescence: no unevaluated settle, no entry in flight, and no
    /// instance queued or awaiting settlement — except that an admitted
    /// resident instance (alive until `stop()`) never blocks completion,
    /// while a resident's still-queued instance does.
    pub(super) fn run_complete(&self) -> bool {
        if !self.pending_evaluations.is_empty() {
            return false;
        }
        for state in &self.states {
            let resident = state.resident();
            for entry in &state.entries {
                if entry.status.in_flight() && !(resident && entry.status == Status::Running) {
                    return false;
                }
                for instance in &entry.instances {
                    if instance.status == Status::Pending {
                        return false;
                    }
                    if instance.status == Status::Running
                        && !(resident && entry.status == Status::Running)
                    {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn run_budget_exceeded(&self, now: f64) -> bool {
        match self.run_budget_ms {
            Some(budget) if !self.budget_reported => {
                (now - self.started_at) * 1000.0 > budget as f64
            }
            _ => false,
        }
    }

    fn children_budget_exceeded(&self) -> bool {
        !self.max_children_reported && self.spawn_count >= self.max_children
    }

    fn loop_alive(&self, generation: u64) -> bool {
        self.state == RunState::Running && self.loop_generation == generation
    }

    fn backoff_active(&self, now: f64) -> bool {
        self.admission_backoff_until
            .is_some_and(|until| now < until)
    }

    /// The first pending instance of a RUNNING entry: a terminal entry's
    /// queued siblings are never admitted.
    fn next_pending_instance(&self) -> Option<Slot> {
        for (state_position, state) in self.states.iter().enumerate() {
            for (entry_position, entry) in state.entries.iter().enumerate() {
                if entry.status != Status::Running {
                    continue;
                }
                if let Some(instance) = entry
                    .instances
                    .iter()
                    .position(|instance| instance.status == Status::Pending)
                {
                    return Some(Slot {
                        state: state_position,
                        entry: entry_position,
                        instance,
                    });
                }
            }
        }
        None
    }

    /// Every `max_parallel` slot is held by a never-settling resident
    /// instance while instances wait behind the cap.
    fn resident_cap_starved(&self) -> bool {
        let running: Vec<bool> = self
            .instances()
            .filter(|(_, _, instance)| instance.status == Status::Running)
            .map(|(state, _, _)| state.resident())
            .collect();
        if (running.len() as u64) < self.max_parallel || !self.has_pending_instance() {
            return false;
        }
        running.iter().all(|resident| *resident)
    }

    /// Bind inputs, expand foreach, and render one prompt per instance:
    /// `Ok(Some(instances))`, `Ok(None)` while an input source has not
    /// settled yet (the entry stays pending), or `Err(reason)` on a binding
    /// failure (never retried: it would recur on every re-render).
    fn prepare_entry(&mut self, position: usize) -> Result<Option<Vec<NodeInstance>>, String> {
        let spec = self.states[position].spec.clone();
        let foreach = spec.get("foreach").filter(|foreach| !foreach.is_null());
        let over = foreach
            .and_then(|foreach| foreach.get("over"))
            .and_then(Value::as_str);
        let mut values: Vec<(String, String)> = Vec::new();
        let mut items: Option<Vec<Value>> = None;
        let inputs = spec
            .get("inputs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for input in &inputs {
            let name = input
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let port_type = input
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let source = input
                .get("from")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (src_id, src_output) = source.split_once('.').unwrap_or((source, ""));
            let latest = self
                .position(src_id)
                .and_then(|src| self.states[src].latest_settle());
            let mut value: Option<Value> = None;
            let mut failure: Option<String> = None;
            if let Some(latest) = latest {
                if latest.status == Status::Error {
                    failure = Some(format!(
                        "input {} from state {} is unavailable (latest settle status 'error')",
                        py_str_repr(name),
                        py_str_repr(src_id)
                    ));
                } else {
                    let errors = latest.output_errors.as_ref();
                    let outputs = latest.outputs.as_ref();
                    if let Some(error) = errors.and_then(|errors| errors.get(src_output)) {
                        failure = Some(format!(
                            "input {}: {}",
                            py_str_repr(name),
                            error.as_str().unwrap_or_default()
                        ));
                    } else if let Some(captured) =
                        outputs.and_then(|outputs| outputs.get(src_output))
                    {
                        value = Some(captured.clone());
                    } else {
                        failure = Some(format!(
                            "input {} from state {} has no captured output {}",
                            py_str_repr(name),
                            py_str_repr(src_id),
                            py_str_repr(src_output)
                        ));
                    }
                }
            }
            let Some(value) = value.filter(|_| latest.is_some() && failure.is_none()) else {
                let optional = input.get("optional").is_some_and(json_truthy);
                // An optional input over a DIFFERENT state with a live entry
                // waits for that source's settle: a sentinel here would spawn
                // the dependent beside its running upstream (upstream #3462's
                // M3). The self-input loop form and a source with no live
                // entry keep the sentinel.
                if optional && latest.is_none() && src_id != self.states[position].state_id {
                    let source_live = self.position(src_id).is_some_and(|src| {
                        self.states[src]
                            .entries
                            .iter()
                            .any(|entry| entry.status.in_flight())
                    });
                    if source_live {
                        return Ok(None);
                    }
                }
                if optional {
                    // Optional inputs bind a null sentinel whenever their
                    // source offers no value; an optional foreach.over
                    // expands to zero items instead.
                    if over == Some(name) {
                        items = Some(Vec::new());
                        continue;
                    }
                    let sentinel = if port_type == "json" { "null" } else { "None" };
                    values.push((name.to_string(), sentinel.to_string()));
                    continue;
                }
                if latest.is_none() {
                    return Ok(None);
                }
                return Err(failure.unwrap_or_default());
            };
            if port_type == "text" {
                values.push((name.to_string(), text_of(&value)));
                continue;
            }
            if over == Some(name) {
                let Value::Array(list) = value else {
                    return Err(format!(
                        "foreach.over input {} is not a JSON list",
                        py_str_repr(name)
                    ));
                };
                items = Some(list);
                continue;
            }
            values.push((name.to_string(), py_json_dumps(&value)));
        }
        let state = &mut self.states[position];
        let template = state.prompt_template.clone().unwrap_or_default();
        let Some(foreach) = foreach else {
            let instance =
                NodeInstance::new(state.instance_counter, render_prompt(&template, &values));
            state.instance_counter += 1;
            return Ok(Some(vec![instance]));
        };
        let Some(items) = items else {
            return Err("foreach entry did not resolve its over input".to_string());
        };
        let max = foreach.get("max").and_then(Value::as_u64).unwrap_or(0) as usize;
        let over = over.unwrap_or_default().to_string();
        let mut instances = Vec::new();
        for item in items.iter().take(max) {
            let mut bound = values.clone();
            let item_text = text_of(item);
            match bound.iter_mut().find(|(name, _)| *name == over) {
                Some(existing) => existing.1 = item_text,
                None => bound.push((over.clone(), item_text)),
            }
            let index = state.instance_counter + instances.len() as u64;
            instances.push(NodeInstance::new(index, render_prompt(&template, &bound)));
        }
        state.instance_counter += instances.len() as u64;
        Ok(Some(instances))
    }

    /// Capture the state's declared output ports from the entry's answer:
    /// text ports keep the captured string; json ports parse, with the
    /// parse error recorded on the settle.
    fn capture_outputs(&mut self, position: usize, entry: usize) {
        let spec_outputs = self.states[position]
            .spec
            .get("outputs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let target = &mut self.states[position].entries[entry];
        let truncated_at = target
            .answer
            .as_deref()
            .map(|answer| answer.chars().count())
            .filter(|chars| *chars >= ANSWER_BINDING_CAP);
        let mut outputs = Map::new();
        let mut errors = Map::new();
        for out in &spec_outputs {
            let name = out.get("name").and_then(Value::as_str).unwrap_or_default();
            let port_type = out.get("type").and_then(Value::as_str).unwrap_or_default();
            let Some(answer) = target.answer.as_deref() else {
                continue;
            };
            if port_type == "text" {
                outputs.insert(name.to_string(), Value::from(answer));
                continue;
            }
            match parse_json_output(answer, name) {
                Ok(parsed) => {
                    outputs.insert(name.to_string(), parsed);
                }
                Err(mut error) => {
                    // A capture cut at the binding cap reads as a size
                    // problem, not a missing output.
                    if let Some(chars) = truncated_at {
                        let _ = write!(
                            error,
                            "; the captured answer is truncated at {chars} characters - keep the fenced JSON block compact"
                        );
                    }
                    errors.insert(name.to_string(), Value::from(error));
                }
            }
        }
        target.outputs = Some(outputs);
        target.output_errors = Some(errors);
    }

    /// The declared output ports the entry's capture bound no value for
    /// (presence, never the value: a port that parsed as JSON null bound).
    fn unbound_outputs(&self, position: usize, entry: usize) -> Vec<String> {
        let target = &self.states[position].entries[entry];
        self.states[position]
            .spec
            .get("outputs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|out| out.get("name").and_then(Value::as_str))
            .filter(|name| {
                !target
                    .outputs
                    .as_ref()
                    .is_some_and(|outputs| outputs.contains_key(*name))
            })
            .map(str::to_string)
            .collect()
    }

    /// Join the entry's done instances' captured answers.
    fn join_entry_answer(&mut self, position: usize, entry: usize) {
        let target = &mut self.states[position].entries[entry];
        let answers: Vec<&str> = target
            .instances
            .iter()
            .filter(|instance| instance.status == Status::Done)
            .filter_map(|instance| instance.answer.as_deref())
            .collect();
        target.answer = (!answers.is_empty()).then(|| answers.join("\n\n"));
    }
}

/// Python truthiness of a JSON value.
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// A milestone queued inside a run lock, delivered after it is released.
struct MilestoneCall {
    kind: &'static str,
    detail: String,
    node: Option<String>,
}

impl Inner {
    /// Record a run milestone and inject one quiet notice (one per kind).
    /// Every milestone lands in the ledger, repeats included; only the
    /// parent-visible notice is deduped per kind.
    async fn milestone(&self, cell: &Arc<RunCell>, announce: MilestoneCall) {
        let notice = cell.with(|run| {
            let seq = run.record(
                kind::MILESTONE,
                EventAt {
                    node: announce.node.as_deref(),
                    entry: None,
                    instance: None,
                    detail: Some(&announce.detail),
                },
                vec![("milestone", Value::from(announce.kind))],
            );
            if !run.milestones.insert(announce.kind.to_string()) {
                return None;
            }
            let mut payload = Map::new();
            payload.insert("run_id".into(), Value::from(run.run_id.clone()));
            payload.insert("kind".into(), Value::from(announce.kind));
            payload.insert("detail".into(), Value::from(announce.detail.clone()));
            if let Some(node) = &announce.node {
                payload.insert("node".into(), Value::from(node.clone()));
            }
            Some((seq, Value::Object(payload)))
        });
        let Some((seq, payload)) = notice else {
            return;
        };
        if self.notices.notify(payload).await.is_ok() {
            cell.with(|run| {
                if let Some(event) = run.events.get_mut(seq as usize - 1) {
                    event["stage"] = Value::from("shown");
                }
                run.touch();
            });
        }
    }

    /// Register a run, enter its entry states, and start the control loop.
    /// Nonblocking: admission enters every entry state up to `max_parallel`
    /// and returns; a background task continues the run.
    pub(super) async fn launch_run(self: &Arc<Self>, cell: &Arc<RunCell>) -> Value {
        let generation = cell.with(|run| {
            let detail = format!(
                "{} states, max_parallel {}",
                run.states.len(),
                run.max_parallel
            );
            run.record(
                kind::RUN_STARTED,
                EventAt {
                    detail: Some(&detail),
                    ..EventAt::none()
                },
                Vec::new(),
            );
            for position in 0..run.states.len() {
                if run.states[position]
                    .spec
                    .get("entry")
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    run.enter_state(position, None);
                }
            }
            run.loop_generation
        });
        let started = self
            .spawn_ready(cell, generation, /*allow_backoff*/ false)
            .await;
        if cell.with(|run| run.run_complete()) {
            self.finalize(cell).await;
        } else if cell.with(|run| run.state == RunState::Running) {
            // A budget pause during the initial admission leaves the run
            // paused with no loop: resuming is the operator's decision.
            self.start_loop(cell);
        }
        cell.with(|run| {
            let mut result = Map::new();
            result.insert("run_id".into(), Value::from(run.run_id.clone()));
            result.insert("spec_id".into(), Value::from(run.spec_id.clone()));
            result.insert(
                "name".into(),
                run.name.clone().map_or(Value::Null, Value::from),
            );
            result.insert("nodes".into(), Value::from(run.states.len()));
            result.insert("max_parallel".into(), Value::from(run.max_parallel));
            result.insert("started".into(), Value::from(started));
            result.insert("pending".into(), Value::from(run.pending_state_ids()));
            Value::Object(result)
        })
    }

    /// Cancel every running child and mark the run stopped. The
    /// transitional `stopping` state is set before the first await, so the
    /// control loop cannot admit or finalize meanwhile; idempotent.
    pub(super) async fn stop(&self, cell: &Arc<RunCell>) -> Value {
        let already = cell.with(|run| {
            if matches!(run.state, RunState::Stopping | RunState::Stopped) {
                return Some(serde_json::json!({
                    "run_id": run.run_id,
                    "state": run.state.as_str(),
                    "cancelled": [],
                }));
            }
            run.state = RunState::Stopping;
            run.touch();
            None
        });
        if let Some(result) = already {
            return result;
        }
        let stopped = self.halt_nonterminal(cell, "run stopped").await;
        cell.with(|run| {
            run.state = RunState::Stopped;
            let detail = format!("stopped; {} state(s) cancelled", stopped.len());
            run.record(
                kind::RUN_STOPPED,
                EventAt {
                    detail: Some(&detail),
                    ..EventAt::none()
                },
                Vec::new(),
            );
            serde_json::json!({ "run_id": run.run_id, "state": "stopped", "cancelled": stopped })
        })
    }

    /// Resume a paused run (escalate, budget, `max_transitions`,
    /// `max_children`, or a host-restart interruption).
    pub(super) async fn resume(self: &Arc<Self>, cell: &Arc<RunCell>) -> Result<Value, String> {
        let (generation, lost) = cell.with(|run| {
            if run.state != RunState::Paused {
                return Err(format!(
                    "factory run {} is {}, not paused",
                    py_str_repr(&run.run_id),
                    py_str_repr(run.state.as_str())
                ));
            }
            // Bump the generation FIRST: a winding-down pause-path loop
            // must never continue as a second concurrent control loop.
            run.loop_generation += 1;
            run.state = RunState::Running;
            run.pause_reason = None;
            run.touch();
            run.record(
                kind::RESUMED,
                EventAt {
                    detail: Some("resumed by caller"),
                    ..EventAt::none()
                },
                Vec::new(),
            );
            let mut lost = Vec::new();
            for state in &mut run.states {
                for entry in &mut state.entries {
                    for instance in &mut entry.instances {
                        if let Some(child) = instance.interrupted_child.take() {
                            lost.push(child);
                        }
                    }
                }
            }
            Ok((run.loop_generation, lost))
        })?;
        // A host restart lost these children mid-flight: retire them
        // best-effort before their instances re-admit.
        for child in lost {
            let _ = self.children.delete(child).await;
        }
        // Settles first: a paused run may still carry transitions to fire.
        self.evaluate_settles(cell).await;
        let started = self
            .spawn_ready(cell, generation, /*allow_backoff*/ false)
            .await;
        if cell.with(|run| run.run_complete()) {
            self.finalize(cell).await;
        } else if cell.with(|run| run.state == RunState::Running) {
            self.start_loop(cell);
        }
        Ok(cell.with(|run| {
            serde_json::json!({
                "run_id": run.run_id,
                "state": run.state.as_str(),
                "started": started,
                "pending": run.pending_state_ids(),
            })
        }))
    }

    /// Evaluate every queued settle's outgoing transitions once: every
    /// passing guard fires (fan-out is legal), a join fires once per
    /// source-settle combination, a fire enters the target unless it is
    /// out of `max_entries` (`transition_blocked`). Exceeding
    /// `max_transitions` pauses the run once with the settle unconsumed and
    /// the transition index recorded, so resume never re-fires.
    async fn evaluate_settles(&self, cell: &Arc<RunCell>) {
        let pause = cell.with(|run| {
            while run.state == RunState::Running {
                let Some(pending) = run.pending_evaluations.pop_front() else {
                    break;
                };
                let Some(position) = run.position(&pending.state_id) else {
                    continue;
                };
                let entry_index = pending.entry;
                {
                    let entry = &mut run.states[position].entries[entry_index];
                    if entry.consumed || !entry.is_settle {
                        continue;
                    }
                    entry.consumed = true;
                }
                let outputs = run.states[position].entries[entry_index]
                    .outputs
                    .clone()
                    .unwrap_or_default();
                let transitions = run
                    .transitions_from
                    .get(&pending.state_id)
                    .cloned()
                    .unwrap_or_default();
                for (local_index, transition_index) in transitions.iter().enumerate() {
                    if local_index < pending.resume_from {
                        continue;
                    }
                    let transition = run.transition(*transition_index).clone();
                    let from_field = transition.get("from").cloned().unwrap_or(Value::Null);
                    let target_id = transition
                        .get("to")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let mut join: Option<JoinMark> = None;
                    if let Value::Array(sources) = &from_field {
                        let mut signature = Vec::with_capacity(sources.len());
                        let mut complete = true;
                        for source in sources.iter().filter_map(Value::as_str) {
                            let latest = run
                                .position(source)
                                .and_then(|src| run.states[src].latest_settle())
                                .map(|entry| entry.index);
                            if let Some(index) = latest {
                                signature.push((source.to_string(), index));
                            } else {
                                complete = false;
                                break;
                            }
                        }
                        if !complete {
                            continue;
                        }
                        signature.sort();
                        let already = run.join_fired.iter().any(|mark| {
                            mark.transition == *transition_index
                                && mark.to == target_id
                                && mark.signature == signature
                        });
                        if already {
                            continue;
                        }
                        join = Some(JoinMark {
                            transition: *transition_index,
                            to: target_id.clone(),
                            signature,
                        });
                    } else if let Some(when) = transition.get("when").filter(|when| !when.is_null()) {
                        if !guard_passes(when, &outputs) {
                            continue;
                        }
                    }
                    let Some(target) = run.position(&target_id) else {
                        continue;
                    };
                    let claim = |run: &mut FactoryRun, join: Option<JoinMark>| {
                        if let Some(mark) = join {
                            run.join_fired
                                .retain(|old| !(old.transition == mark.transition && old.to == mark.to));
                            run.join_fired.push(mark);
                        }
                    };
                    let (entries_used, max_entries) =
                        (run.states[target].entries_used, run.states[target].max_entries);
                    if entries_used >= max_entries {
                        claim(run, join);
                        let detail = format!(
                            "state {} is at max_entries {max_entries}; transition {} -> {} blocked",
                            py_str_repr(&target_id),
                            json_repr(&from_field),
                            py_str_repr(&target_id)
                        );
                        run.record(
                            kind::TRANSITION_BLOCKED,
                            EventAt {
                                detail: Some(&detail),
                                ..EventAt::none()
                            },
                            vec![("from", from_field.clone()), ("to", Value::from(target_id.clone()))],
                        );
                        continue;
                    }
                    if run.transitions_fired >= run.max_transitions && !run.max_transitions_reported {
                        run.max_transitions_reported = true;
                        run.states[position].entries[entry_index].consumed = false;
                        run.pending_evaluations.push_front(PendingEvaluation {
                            state_id: pending.state_id.clone(),
                            entry: entry_index,
                            resume_from: local_index,
                        });
                        run.state = RunState::Paused;
                        run.pause_reason = Some("max_transitions exceeded".to_string());
                        return Some(MilestoneCall {
                            kind: "max_transitions_exceeded",
                            detail: format!(
                                "max_transitions {} exceeded; no new entries; resume with await rlm.factory.resume('{}')",
                                run.max_transitions, run.run_id
                            ),
                            node: None,
                        });
                    }
                    claim(run, join);
                    run.transitions_fired += 1;
                    let mut fired = vec![("from", from_field.clone()), ("to", Value::from(target_id.clone()))];
                    if let Some(when) = transition.get("when").filter(|when| !when.is_null()) {
                        fired.push(("when", when.clone()));
                    }
                    let detail = format!("{} -> {}", json_repr(&from_field), py_str_repr(&target_id));
                    run.record(
                        kind::TRANSITION_FIRED,
                        EventAt {
                            detail: Some(&detail),
                            ..EventAt::none()
                        },
                        fired,
                    );
                    run.enter_state(target, Some(&pending.state_id));
                }
            }
            None
        });
        if let Some(announce) = pause {
            self.milestone(cell, announce).await;
        }
    }

    /// Prepare ready entries and admit pending instances up to
    /// `max_parallel`. The run budget and the run-wide child budget are
    /// enforced BEFORE each admission (this phase runs from `run()` and
    /// `resume()` too); admission is skipped while a rate-limit backoff
    /// deadline is outstanding. Answers the states that had an instance
    /// admitted here.
    async fn spawn_ready(
        &self,
        cell: &Arc<RunCell>,
        generation: u64,
        allow_backoff: bool,
    ) -> Vec<String> {
        enum Next {
            Stop,
            PauseBudget,
            PauseChildren,
            Admit(Slot),
        }
        let mut started: Vec<String> = Vec::new();
        while cell.with(|run| run.loop_alive(generation)) {
            self.prepare_ready_entries(cell).await;
            let now = self.clock.now();
            let next = cell.with(|run| {
                if !run.loop_alive(generation) {
                    return Next::Stop;
                }
                if run.run_budget_exceeded(now) {
                    return Next::PauseBudget;
                }
                if run.backoff_active(now) {
                    return Next::Stop;
                }
                if run.running_instance_count() as u64 >= run.max_parallel {
                    return Next::Stop;
                }
                let Some(slot) = run.next_pending_instance() else {
                    return Next::Stop;
                };
                if run.children_budget_exceeded() {
                    return Next::PauseChildren;
                }
                Next::Admit(slot)
            });
            match next {
                Next::Stop => break,
                Next::PauseBudget => {
                    self.pause_for_budget(cell).await;
                    break;
                }
                Next::PauseChildren => {
                    self.pause_for_children(cell).await;
                    break;
                }
                Next::Admit(slot) => {
                    let state_id = cell.with(|run| run.states[slot.state].state_id.clone());
                    match self.admit(cell, slot, allow_backoff).await {
                        Admission::Admitted => {
                            if !started.contains(&state_id) {
                                started.push(state_id);
                            }
                        }
                        // The run left "running" mid-admission, or a rate
                        // limit (usually global) deferred admission.
                        Admission::Stopped | Admission::Deferred => break,
                        // The failure policy owns the run state now; the
                        // loop condition re-checks it.
                        Admission::Failed => {}
                    }
                }
            }
        }
        started
    }

    /// Pause at the budget boundary (the milestone fires once; resuming is
    /// an explicit operator decision, so no further budget pauses fire).
    async fn pause_for_budget(&self, cell: &Arc<RunCell>) {
        let now = self.clock.now();
        let announce = cell.with(|run| {
            let elapsed_ms = (now - run.started_at) * 1000.0;
            run.state = RunState::Paused;
            run.pause_reason = Some("run budget exceeded".to_string());
            run.budget_reported = true;
            run.touch();
            MilestoneCall {
                kind: "budget_exceeded",
                detail: format!(
                    "run budget_ms {} exceeded after {}ms; no new spawns; resume with await rlm.factory.resume('{}')",
                    run.run_budget_ms.unwrap_or_default(),
                    elapsed_ms as i64,
                    run.run_id
                ),
                node: None,
            }
        });
        self.milestone(cell, announce).await;
    }

    /// Pause at the child-budget boundary: `max_children` bounds every
    /// admission over the run's life (foreach expansions and retries
    /// included); the milestone fires once.
    async fn pause_for_children(&self, cell: &Arc<RunCell>) {
        let announce = cell.with(|run| {
            run.state = RunState::Paused;
            run.pause_reason = Some("max_children exceeded".to_string());
            run.max_children_reported = true;
            run.touch();
            MilestoneCall {
                kind: "max_children_exceeded",
                detail: format!(
                    "run max_children {} exceeded after {} children; no new spawns; resume with await rlm.factory.resume('{}')",
                    run.max_children, run.spawn_count, run.run_id
                ),
                node: None,
            }
        });
        self.milestone(cell, announce).await;
    }

    /// Bind inputs and create instances for every entry whose input
    /// sources have settles.
    async fn prepare_ready_entries(&self, cell: &Arc<RunCell>) {
        let mut position = 0;
        loop {
            let mut entry_index = 0;
            loop {
                enum Prepared {
                    Done,
                    NextEntry,
                    Failed(String),
                }
                let prepared = cell.with(|run| {
                    let Some(state) = run.states.get(position) else {
                        return Prepared::Done;
                    };
                    let Some(entry) = state.entries.get(entry_index) else {
                        return Prepared::Done;
                    };
                    if entry.status != Status::Pending {
                        return Prepared::NextEntry;
                    }
                    match run.prepare_entry(position) {
                        Err(reason) => Prepared::Failed(reason),
                        Ok(None) => Prepared::NextEntry,
                        Ok(Some(instances)) => {
                            let node = run.states[position].state_id.clone();
                            let entry = &mut run.states[position].entries[entry_index];
                            let count = instances.len();
                            let index = entry.index;
                            entry.instances = instances;
                            let detail = if count > 0 {
                                entry.status = Status::Running;
                                format!("{count} instance(s) prepared")
                            } else {
                                entry.status = Status::Done;
                                "foreach expanded to zero items; nothing to run".to_string()
                            };
                            run.record(
                                kind::NODE_READY,
                                EventAt {
                                    node: Some(&node),
                                    entry: Some(index),
                                    instance: None,
                                    detail: Some(&detail),
                                },
                                Vec::new(),
                            );
                            if count == 0 {
                                run.queue_settle(position, entry_index);
                            }
                            Prepared::NextEntry
                        }
                    }
                });
                match prepared {
                    Prepared::Done => break,
                    Prepared::NextEntry => {}
                    Prepared::Failed(reason) => {
                        self.apply_entry_failure_policy(cell, position, entry_index, &reason)
                            .await;
                        if cell.with(|run| run.state != RunState::Running) {
                            return;
                        }
                    }
                }
                entry_index += 1;
            }
            position += 1;
            if cell.with(|run| position >= run.states.len()) {
                return;
            }
        }
    }

    /// Spawn one instance. Rate-limited admissions never sleep here: the
    /// loop path records an exponential backoff deadline (doubling from
    /// 1s, capped at 60s) and defers; the admission phase (`run()`/`resume()`)
    /// defers without one. At most [`BACKOFF_MAX_ATTEMPTS`] consecutive
    /// rate-limited admissions fail the instance; any other admission error
    /// fails it immediately. A spawn that lands after `stop()` is retracted.
    async fn admit(&self, cell: &Arc<RunCell>, slot: Slot, allow_backoff: bool) -> Admission {
        let request = cell.with(|run| {
            let run_id = run.run_id.clone();
            let state = &run.states[slot.state];
            let (name, model, thinking, state_id) = (
                state.name.clone(),
                state.model.clone(),
                state.thinking.clone(),
                state.state_id.clone(),
            );
            let instance = run.slot_instance(slot);
            instance.attempt += 1;
            let label = spawn_label(
                name.as_deref(),
                &run_id,
                &state_id,
                instance.index as i64,
                instance.attempt,
            );
            FactorySpawn {
                prompt: instance.prompt.clone(),
                name: label,
                model,
                thinking,
            }
        });
        let child_name = request.name.clone();
        let spawned = self.children.spawn(request).await;
        let child_id = match spawned {
            Ok(child_id) => child_id,
            Err(error) => {
                let reason = format!("spawn admission failed: {error}");
                if !is_rate_limit_error(&error) {
                    self.apply_instance_failure(cell, slot, reason, /*retry*/ false)
                        .await;
                    return Admission::Failed;
                }
                let now = self.clock.now();
                let exhausted = cell.with(|run| {
                    if !allow_backoff {
                        let detail = format!("rate limited at admission: {error}");
                        run.record_at(kind::SPAWN_DEFERRED, slot, Some(&detail), Vec::new());
                        return false;
                    }
                    let instance = run.slot_instance(slot);
                    instance.rate_limit_streak += 1;
                    let streak = instance.rate_limit_streak;
                    if streak >= BACKOFF_MAX_ATTEMPTS {
                        return true;
                    }
                    let delay = (BACKOFF_BASE_SECONDS * 2f64.powi(streak as i32 - 1))
                        .min(BACKOFF_CAP_SECONDS);
                    let deadline = now + delay;
                    if run
                        .admission_backoff_until
                        .is_none_or(|until| deadline > until)
                    {
                        run.admission_backoff_until = Some(deadline);
                    }
                    let detail = format!("rate limited; retrying in {}s", seconds_text(delay));
                    run.record_at(kind::SPAWN_BACKOFF, slot, Some(&detail), Vec::new());
                    false
                });
                if exhausted {
                    self.apply_instance_failure(cell, slot, reason, /*retry*/ false)
                        .await;
                    return Admission::Failed;
                }
                return Admission::Deferred;
            }
        };
        let now = self.clock.now();
        let stopped = cell.with(|run| {
            let running = run.state == RunState::Running;
            let instance = run.slot_instance(slot);
            instance.child_id = Some(child_id.clone());
            if !running {
                return true;
            }
            instance.spawned_at = Some(now);
            instance.status = Status::Running;
            instance.rate_limit_streak = 0;
            let attempt = instance.attempt;
            run.spawn_count += 1;
            run.record_at(
                kind::SPAWNED,
                slot,
                None,
                vec![
                    ("attempt", Value::from(attempt)),
                    ("child", Value::from(child_id.clone())),
                    ("name", Value::from(child_name.clone())),
                ],
            );
            false
        });
        if stopped {
            // stop() ran while the spawn was in flight: its cancellation
            // pass could not see this child, so retract it here.
            self.retract_admission(cell, slot).await;
            return Admission::Stopped;
        }
        Admission::Admitted
    }

    async fn apply_settlement(&self, cell: &Arc<RunCell>, slot: Slot, result: &RlmChildResult) {
        enum Outcome {
            Ignore,
            Fail(String, bool),
            Done,
            /// The entry settled but a declared output bound nothing: its
            /// settled children are re-collected once before the settle.
            CaptureRetry(Vec<String>),
        }
        let now = self.clock.now();
        let outcome = cell.with(|run| {
            let budget_ms = run.states[slot.state].spec.get("budget_ms").and_then(Value::as_u64);
            let instance = run.slot_instance(slot);
            if instance.status != Status::Running {
                return Outcome::Ignore; // cancelled while the collect was in flight
            }
            instance.duration_ms = result.duration_ms;
            instance.tool_uses = result.tool_use_count.unwrap_or(0);
            let tool_uses = instance.tool_uses;
            let spawned_at = instance.spawned_at;
            run.settle_count += 1;
            run.tool_use_total += tool_uses;
            let child_reason = match result.status {
                "done" => None,
                "error" => Some(
                    result
                        .error
                        .clone()
                        .filter(|error| !error.is_empty())
                        .unwrap_or_else(|| "child settled with status 'error'".to_string()),
                ),
                "cancelled" => Some("child was cancelled".to_string()),
                other => Some(format!("child settled with unexpected status {}", py_str_repr(other))),
            };
            if let Some(reason) = child_reason {
                // The exit capture (upstream #3462's M4): whatever the exit
                // envelope carried is a PROVISIONAL answer, kept on the
                // instance so the state reads needs-verify instead of the
                // exit silently counting as settled work.
                let exit_answer = binding_lane(result);
                if exit_answer.is_some() {
                    let instance = run.slot_instance(slot);
                    instance.answer = exit_answer;
                    instance.provisional = true;
                }
                // Child failures retry (same prompt, attempts + 1) while
                // attempts remain; then the entry failure policy applies.
                return Outcome::Fail(reason, true);
            }
            if let (Some(budget_ms), Some(spawned_at)) = (budget_ms, spawned_at) {
                let elapsed_ms = (now - spawned_at) * 1000.0;
                if elapsed_ms > budget_ms as f64 {
                    return Outcome::Fail(
                        format!(
                            "state budget_ms {budget_ms} exceeded ({}ms from admission to settlement)",
                            elapsed_ms as i64
                        ),
                        false,
                    );
                }
            }
            let instance = run.slot_instance(slot);
            instance.status = Status::Done;
            // The binding lane first (the collect envelope's full answer);
            // the roster preview is the fallback. A real settle supersedes
            // any exit capture.
            instance.answer = binding_lane(result);
            instance.provisional = false;
            let duration = instance.duration_ms;
            let captured = instance
                .answer
                .as_deref()
                .map(|answer| char_prefix(answer, ANSWER_CAPTURE_CAP).to_string());
            run.record_at(
                kind::SETTLED,
                slot,
                None,
                vec![
                    ("status", Value::from("done")),
                    ("duration_ms", duration.map_or(Value::Null, Value::from)),
                ],
            );
            if let Some(answer) = captured {
                let at = run.slot_at(slot);
                let (node, entry, instance) = (at.node.map(str::to_string), at.entry, at.instance);
                run.event(
                    kind::ANSWER_CAPTURED,
                    EventAt {
                        node: node.as_deref(),
                        entry,
                        instance,
                        detail: None,
                    },
                    "arrived",
                    vec![("answer", Value::from(answer))],
                );
            }
            let entry = &mut run.states[slot.state].entries[slot.entry];
            if entry.status == Status::Running
                && !entry.instances.is_empty()
                && entry.instances.iter().all(|instance| instance.status == Status::Done)
            {
                entry.status = Status::Done;
                run.join_entry_answer(slot.state, slot.entry);
                run.capture_outputs(slot.state, slot.entry);
                if !run.unbound_outputs(slot.state, slot.entry).is_empty() {
                    let children: Vec<String> = run.states[slot.state].entries[slot.entry]
                        .instances
                        .iter()
                        .filter(|instance| instance.status == Status::Done)
                        .filter_map(|instance| instance.child_id.clone())
                        .collect();
                    return Outcome::CaptureRetry(children);
                }
                run.queue_settle(slot.state, slot.entry);
            }
            Outcome::Done
        });
        match outcome {
            Outcome::Fail(reason, retry) => {
                self.apply_instance_failure(cell, slot, reason, retry).await;
            }
            Outcome::CaptureRetry(children) => {
                self.retry_capture(cell, slot, children).await;
            }
            Outcome::Ignore | Outcome::Done => {}
        }
    }

    /// The settle capture's one retry (upstream #3462's M2): the host can
    /// settle a child before its answer capture lands (a later refresh
    /// recovers it), so a declared output that bound nothing re-collects
    /// the entry's settled children once and re-captures; a port that
    /// still binds nothing records `output_capture_failed` instead of
    /// passing silently. The settle is queued either way.
    async fn retry_capture(&self, cell: &Arc<RunCell>, slot: Slot, children: Vec<String>) {
        let refreshed = if children.is_empty() {
            Ok(Vec::new())
        } else {
            self.children.collect(children, 0).await
        };
        cell.with(|run| {
            match refreshed {
                Ok(results) => {
                    let mut changed = false;
                    for instance in &mut run.states[slot.state].entries[slot.entry].instances {
                        let Some(result) = results.iter().find(|result| {
                            Some(result.rlm_child_id.as_str()) == instance.child_id.as_deref()
                        }) else {
                            continue;
                        };
                        let Some(answer) = binding_lane(result) else {
                            continue;
                        };
                        let longer = instance
                            .answer
                            .as_deref()
                            .is_none_or(|current| answer.chars().count() > current.chars().count());
                        if longer {
                            instance.answer = Some(answer);
                            changed = true;
                        }
                    }
                    if changed {
                        run.join_entry_answer(slot.state, slot.entry);
                        run.capture_outputs(slot.state, slot.entry);
                    }
                }
                Err(error) => {
                    let node = run.states[slot.state].state_id.clone();
                    let index = run.states[slot.state].entries[slot.entry].index;
                    run.record(
                        kind::OUTPUT_CAPTURE_FAILED,
                        EventAt {
                            node: Some(&node),
                            entry: Some(index),
                            instance: None,
                            detail: Some("the first capture stands"),
                        },
                        vec![(
                            "error",
                            Value::from(format!(
                                "capture retry could not re-collect the settled children: {error}"
                            )),
                        )],
                    );
                }
            }
            let node = run.states[slot.state].state_id.clone();
            let index = run.states[slot.state].entries[slot.entry].index;
            for port in run.unbound_outputs(slot.state, slot.entry) {
                let error = run.states[slot.state].entries[slot.entry]
                    .output_errors
                    .as_ref()
                    .and_then(|errors| errors.get(&port))
                    .and_then(Value::as_str)
                    .map_or_else(
                        || {
                            format!(
                                "output {} captured no value from the upstream answer",
                                py_str_repr(&port)
                            )
                        },
                        str::to_string,
                    );
                let detail = format!(
                    "output {} did not bind after one capture retry",
                    py_str_repr(&port)
                );
                run.record(
                    kind::OUTPUT_CAPTURE_FAILED,
                    EventAt {
                        node: Some(&node),
                        entry: Some(index),
                        instance: None,
                        detail: Some(&detail),
                    },
                    vec![("port", Value::from(port)), ("error", Value::from(error))],
                );
            }
            run.queue_settle(slot.state, slot.entry);
        });
    }

    async fn apply_instance_failure(
        &self,
        cell: &Arc<RunCell>,
        slot: Slot,
        reason: String,
        retry: bool,
    ) {
        let retried = cell.with(|run| {
            let retries = run.states[slot.state].spec_u64("retries", NODE_RETRIES_DEFAULT as u64);
            let entry_running =
                run.states[slot.state].entries[slot.entry].status == Status::Running;
            let instance = run.slot_instance(slot);
            instance.status = Status::Error;
            instance.error = Some(reason.clone());
            let duration = instance.duration_ms;
            let attempt = instance.attempt;
            run.record_at(
                kind::SETTLED,
                slot,
                None,
                vec![
                    ("status", Value::from("error")),
                    ("error", Value::from(reason.clone())),
                    ("duration_ms", duration.map_or(Value::Null, Value::from)),
                ],
            );
            // A retry is queued only while the entry can re-admit it: a
            // sibling failing after its entry went terminal would sit
            // pending forever.
            if retry && u64::from(attempt) <= retries && entry_running {
                let instance = run.slot_instance(slot);
                instance.status = Status::Pending;
                instance.error = None;
                let detail = format!("attempt {attempt} failed; re-spawning (retries {retries})");
                run.record_at(kind::RETRY, slot, Some(&detail), Vec::new());
                return true;
            }
            false
        });
        if !retried {
            // The instance failed permanently, so the entry fails NOW (a
            // foreach entry does not wait for its remaining instances).
            self.apply_entry_failure_policy(cell, slot.state, slot.entry, &reason)
                .await;
        }
    }

    async fn apply_entry_failure_policy(
        &self,
        cell: &Arc<RunCell>,
        position: usize,
        entry_index: usize,
        reason: &str,
    ) {
        let policy = cell.with(|run| {
            // The verify mark is oversight, not failure-policy bookkeeping:
            // a later foreach sibling can exit with a provisional answer
            // AFTER its entry went terminal, so the mark runs before the
            // terminal guard; it stays one-shot per entry.
            let marked = &run.states[position].entries[entry_index];
            if !marked.needs_verify
                && marked
                    .instances
                    .iter()
                    .any(|instance| instance.provisional && instance.answer.is_some())
            {
                run.states[position].entries[entry_index].needs_verify = true;
                let node = run.states[position].state_id.clone();
                let index = run.states[position].entries[entry_index].index;
                run.record(
                    kind::NEEDS_VERIFY,
                    EventAt {
                        node: Some(&node),
                        entry: Some(index),
                        instance: None,
                        detail: Some(
                            "child exit captured a provisional answer - verify the remote state",
                        ),
                    },
                    Vec::new(),
                );
            }
            let state = &run.states[position];
            if state.entries[entry_index].status.terminal() {
                return None; // the policy already ran for this entry
            }
            let policy = state.failure_policy().to_string();
            let node = state.state_id.clone();
            let state = &mut run.states[position];
            state.error = Some(reason.to_string());
            let entry = &mut state.entries[entry_index];
            entry.status = Status::Error;
            entry.error = Some(reason.to_string());
            let index = entry.index;
            run.last_error = Some(format!("state {} failed: {reason}", py_str_repr(&node)));
            let detail = format!("failure_policy {policy}");
            run.record(
                kind::NODE_ERROR,
                EventAt {
                    node: Some(&node),
                    entry: Some(index),
                    instance: None,
                    detail: Some(&detail),
                },
                vec![("error", Value::from(reason))],
            );
            // The entry is terminal: its prepared-but-never-admitted
            // instances can never run now.
            let pending: Vec<usize> = run.states[position].entries[entry_index]
                .instances
                .iter()
                .enumerate()
                .filter(|(_, instance)| instance.status == Status::Pending)
                .map(|(instance, _)| instance)
                .collect();
            for instance in pending {
                let slot = Slot {
                    state: position,
                    entry: entry_index,
                    instance,
                };
                run.slot_instance(slot).status = Status::Cancelled;
                run.record_at(
                    kind::CANCELLED,
                    slot,
                    Some("entry failed before admission"),
                    Vec::new(),
                );
            }
            // The failed entry settles too: guard-less transitions (the
            // compiled dag's depends_on edges) fire from error settles.
            run.queue_settle(position, entry_index);
            if run.state != RunState::Running {
                return None; // stop() (or another transition) owns the run
            }
            Some((policy, node))
        });
        let Some((policy, node)) = policy else {
            return;
        };
        match policy.as_str() {
            "fail_fast" => {
                self.halt_nonterminal(cell, "run failed (fail_fast)").await;
                let announce = cell.with(|run| {
                    if run.state != RunState::Running {
                        return None; // stop() landed during the cancellations
                    }
                    let cancelled = run
                        .instances()
                        .filter(|(_, _, instance)| instance.status == Status::Cancelled)
                        .count();
                    run.state = RunState::Failed;
                    run.touch();
                    Some(MilestoneCall {
                        kind: "failed",
                        detail: format!("state {node} failed: {reason}; cancelled {cancelled} in-flight child(ren)"),
                        node: Some(node.clone()),
                    })
                });
                if let Some(announce) = announce {
                    self.milestone(cell, announce).await;
                }
            }
            "continue" => {}
            _ => {
                let announce = cell.with(|run| {
                    run.state = RunState::Paused;
                    run.pause_reason = Some(reason.to_string());
                    run.touch();
                    MilestoneCall {
                        kind: "paused",
                        detail: format!(
                            "state {node} failed: {reason}; resume with await rlm.factory.resume('{}')",
                            run.run_id
                        ),
                        node: Some(node.clone()),
                    }
                });
                self.milestone(cell, announce).await;
            }
        }
    }

    /// Delete every running child and cancel every non-terminal entry;
    /// never-entered states are marked cancelled. A state participates when
    /// ANY entry is non-terminal (a re-entered state can keep an earlier
    /// entry in flight).
    async fn halt_nonterminal(&self, cell: &Arc<RunCell>, reason: &str) -> Vec<String> {
        let stopped: Vec<String> = cell.with(|run| {
            run.states
                .iter()
                .filter(|state| {
                    state.entries.iter().any(|entry| entry.status.in_flight())
                        || (state.entries.is_empty() && state.status() == Status::Pending)
                })
                .map(|state| state.state_id.clone())
                .collect()
        });
        self.cancel_running(cell).await;
        cell.with(|run| {
            for state_id in &stopped {
                let Some(position) = run.position(state_id) else {
                    continue;
                };
                let state = &run.states[position];
                if !(state.entries.iter().any(|entry| entry.status.in_flight())
                    || state.entries.is_empty())
                {
                    continue;
                }
                run.states[position].cancelled = true;
                for entry_index in 0..run.states[position].entries.len() {
                    let entry = &mut run.states[position].entries[entry_index];
                    if !entry.status.in_flight() {
                        continue;
                    }
                    entry.status = Status::Cancelled;
                    let index = entry.index;
                    run.record(
                        kind::NODE_CANCELLED,
                        EventAt {
                            node: Some(state_id),
                            entry: Some(index),
                            instance: None,
                            detail: Some(reason),
                        },
                        Vec::new(),
                    );
                    for instance in 0..run.states[position].entries[entry_index].instances.len() {
                        let slot = Slot {
                            state: position,
                            entry: entry_index,
                            instance,
                        };
                        if run.slot_instance(slot).status == Status::Pending {
                            run.slot_instance(slot).status = Status::Cancelled;
                            run.record_at(
                                kind::CANCELLED,
                                slot,
                                Some("cancelled before admission"),
                                Vec::new(),
                            );
                        }
                    }
                }
            }
        });
        stopped
    }

    /// Cancel an admission that landed after the run left `running`.
    async fn retract_admission(&self, cell: &Arc<RunCell>, slot: Slot) {
        let child_id =
            cell.with(|run| run.slot_instance(slot).child_id.clone().unwrap_or_default());
        let deleted = self.children.delete(child_id.clone()).await;
        cell.with(|run| {
            record_cancellation(run, slot, &child_id, deleted.err());
            run.slot_instance(slot).status = Status::Cancelled;
        });
    }

    /// Delete every running child, claiming each instance before its
    /// delete so a concurrent pass never deletes one child twice.
    async fn cancel_running(&self, cell: &Arc<RunCell>) {
        loop {
            let claimed = cell.with(|run| {
                for (state_position, state) in run.states.iter().enumerate() {
                    for (entry_position, entry) in state.entries.iter().enumerate() {
                        for (instance_position, instance) in entry.instances.iter().enumerate() {
                            if instance.status == Status::Running {
                                if let Some(child) = instance.child_id.clone() {
                                    let slot = Slot {
                                        state: state_position,
                                        entry: entry_position,
                                        instance: instance_position,
                                    };
                                    return Some((slot, child));
                                }
                            }
                        }
                    }
                }
                None
            });
            let Some((slot, child_id)) = claimed else {
                return;
            };
            cell.with(|run| {
                run.slot_instance(slot).status = Status::Cancelled;
                run.touch();
            });
            let deleted = self.children.delete(child_id.clone()).await;
            cell.with(|run| record_cancellation(run, slot, &child_id, deleted.err()));
        }
    }

    async fn finalize(&self, cell: &Arc<RunCell>) {
        let announce = cell.with(|run| {
            if run.state != RunState::Running {
                return None; // stop() or a failure policy owns the final state
            }
            let errors: Vec<String> = run
                .states
                .iter()
                .filter(|state| state.entries.iter().any(|entry| entry.status == Status::Error))
                .map(|state| state.state_id.clone())
                .collect();
            run.touch();
            if !errors.is_empty() {
                run.state = RunState::Failed;
                return Some(MilestoneCall {
                    kind: "failed",
                    detail: format!("completed with state error(s): {}", errors.join(", ")),
                    node: None,
                });
            }
            run.state = RunState::Done;
            let residents = run
                .states
                .iter()
                .filter(|state| {
                    state.resident() && state.entries.iter().any(|entry| entry.status == Status::Running)
                })
                .count();
            let mut detail = format!(
                "run complete: {} state(s), {} transition(s) fired",
                run.states.len(),
                run.transitions_fired
            );
            if residents > 0 {
                let _ = write!(
                    detail,
                    "; {residents} resident state(s) still running (stop with await rlm.factory.stop('{}'))",
                    run.run_id
                );
            }
            Some(MilestoneCall {
                kind: "finished",
                detail,
                node: None,
            })
        });
        if let Some(announce) = announce {
            self.milestone(cell, announce).await;
        }
    }

    /// Start the control loop for the run's current generation.
    pub(super) fn start_loop(self: &Arc<Self>, cell: &Arc<RunCell>) {
        let generation = cell.with(|run| run.loop_generation);
        let inner = Arc::clone(self);
        let task_cell = Arc::clone(cell);
        let handle = tokio::spawn(async move {
            inner.control_loop(&task_cell, generation).await;
        });
        cell.set_task(handle);
    }

    async fn control_loop(&self, cell: &Arc<RunCell>, generation: u64) {
        if let Err(LoopError(error)) = self.loop_body(cell, generation).await {
            // A dead bridge or host failure must not wedge the run
            // silently; a concurrent stop() keeps the final state.
            let announce = cell.with(|run| {
                run.record(
                    kind::EXECUTOR_ERROR,
                    EventAt::none(),
                    vec![("error", Value::from(format!("RuntimeError: {error}")))],
                );
                if run.state != RunState::Running {
                    return None;
                }
                run.state = RunState::Failed;
                Some(MilestoneCall {
                    kind: "failed",
                    detail: format!("executor error: {error}"),
                    node: None,
                })
            });
            if let Some(announce) = announce {
                self.milestone(cell, announce).await;
            }
        }
    }

    /// Fail the run from a stall the loop can never leave.
    async fn fail_stalled(&self, cell: &Arc<RunCell>, reason: String) {
        cell.with(|run| {
            run.record(
                kind::EXECUTOR_ERROR,
                EventAt::none(),
                vec![("error", Value::from(reason.clone()))],
            );
            run.state = RunState::Failed;
        });
        self.milestone(
            cell,
            MilestoneCall {
                kind: "failed",
                detail: reason,
                node: None,
            },
        )
        .await;
    }

    async fn loop_body(&self, cell: &Arc<RunCell>, generation: u64) -> Result<(), LoopError> {
        // The generation re-checks after every await are the resume-race
        // fix: a resume() landing while this task is suspended bumps the
        // generation, so this task exits at its next check.
        while cell.with(|run| run.loop_alive(generation)) {
            let in_flight: Vec<(Slot, String, bool)> = cell.with(|run| {
                let mut slots = Vec::new();
                for (state_position, state) in run.states.iter().enumerate() {
                    for (entry_position, entry) in state.entries.iter().enumerate() {
                        for (instance_position, instance) in entry.instances.iter().enumerate() {
                            if instance.status == Status::Running {
                                if let Some(child) = &instance.child_id {
                                    slots.push((
                                        Slot {
                                            state: state_position,
                                            entry: entry_position,
                                            instance: instance_position,
                                        },
                                        child.clone(),
                                        state.resident(),
                                    ));
                                }
                            }
                        }
                    }
                }
                slots
            });
            if !in_flight.is_empty() {
                let targets = in_flight
                    .iter()
                    .map(|(_, child, _)| child.clone())
                    .collect();
                let results = self
                    .children
                    .collect(targets, POLL_TIMEOUT_MS)
                    .await
                    .map_err(LoopError)?;
                for (slot, child, _) in &in_flight {
                    // A GENERATION bump mid-batch drops this collect's
                    // results (the resumed loop re-collects); a pause or
                    // stop this batch triggered does not.
                    if cell.with(|run| run.loop_generation != generation) {
                        return Ok(());
                    }
                    if let Some(result) = results
                        .iter()
                        .find(|result| result.settled && result.rlm_child_id == *child)
                    {
                        self.apply_settlement(cell, *slot, result).await;
                    }
                }
            }
            // stop() (or a policy transition) can land during the collect;
            // a stopped run must never finalize as done.
            if !cell.with(|run| run.loop_alive(generation)) {
                return Ok(());
            }
            self.evaluate_settles(cell).await;
            if !cell.with(|run| run.loop_alive(generation)) {
                return Ok(());
            }
            if cell.with(|run| run.run_complete()) {
                self.finalize(cell).await;
                return Ok(());
            }
            let now = self.clock.now();
            if cell.with(|run| run.run_budget_exceeded(now)) {
                self.pause_for_budget(cell).await;
                return Ok(());
            }
            let started = self
                .spawn_ready(cell, generation, /*allow_backoff*/ true)
                .await;
            if !cell.with(|run| run.loop_alive(generation)) {
                return Ok(());
            }
            let now = self.clock.now();
            let backoff = cell.with(|run| {
                run.admission_backoff_until
                    .filter(|until| now < *until)
                    .map(|until| until - now)
            });
            if let Some(remaining) = backoff {
                // Wait the deadline out in bounded slices, so children that
                // settle meanwhile are collected on the next iteration.
                self.clock
                    .sleep(remaining.min(POLL_TIMEOUT_MS as f64 / 1000.0))
                    .await;
                continue;
            }
            let (cap_starved, max_parallel) = cell.with(|run| {
                (
                    run.pending_evaluations.is_empty() && run.resident_cap_starved(),
                    run.max_parallel,
                )
            });
            if cap_starved {
                let reason = format!(
                    "control loop stalled: resident instances hold every max_parallel {max_parallel} slot; queued instances can never be admitted"
                );
                self.fail_stalled(cell, reason).await;
                return Ok(());
            }
            let resident_only = in_flight.iter().all(|(_, _, resident)| *resident);
            let stall = cell.with(|run| {
                if (in_flight.is_empty() || resident_only)
                    && started.is_empty()
                    && !run.has_pending_instance()
                    && run.pending_evaluations.is_empty()
                {
                    let stuck: Vec<String> = run
                        .states
                        .iter()
                        .flat_map(|state| {
                            state
                                .entries
                                .iter()
                                .filter(|entry| entry.status == Status::Pending && entry.instances.is_empty())
                                .map(move |_| py_str_repr(&state.state_id))
                        })
                        .collect();
                    return Some(if stuck.is_empty() {
                        "control loop stalled: no in-flight or pending work".to_string()
                    } else {
                        format!(
                            "control loop stalled: pending entry of state {} is waiting for an input source that never settled",
                            stuck.join(", ")
                        )
                    });
                }
                None
            });
            if let Some(reason) = stall {
                self.fail_stalled(cell, reason).await;
                return Ok(());
            }
            // Yield once per iteration: an instantly settling host must not
            // hot-spin the loop and starve other tasks.
            tokio::task::yield_now().await;
        }
        Ok(())
    }
}

/// The ledger rows of one finished cancellation: `cancelled`, preceded by
/// `cancel_failed` when the delete failed (the slot is released either way).
fn record_cancellation(run: &mut FactoryRun, slot: Slot, child_id: &str, failure: Option<String>) {
    match failure {
        Some(error) => {
            run.record_at(
                kind::CANCEL_FAILED,
                slot,
                None,
                vec![
                    ("child", Value::from(child_id)),
                    ("error", Value::from(error)),
                ],
            );
            run.record_at(
                kind::CANCELLED,
                slot,
                Some("slot released despite the failed delete"),
                vec![("child", Value::from(child_id))],
            );
        }
        None => {
            run.record_at(
                kind::CANCELLED,
                slot,
                None,
                vec![("child", Value::from(child_id))],
            );
        }
    }
}

/// The settle capture's binding lane: the collect envelope's full final
/// answer, else the roster preview (older hosts, a capture that raced a
/// worker teardown), capped at [`ANSWER_BINDING_CAP`]; `None` when empty.
fn binding_lane(result: &RlmChildResult) -> Option<String> {
    let answer = result
        .answer_text
        .as_deref()
        .filter(|text| !text.is_empty())
        .or(result.answer_preview.as_deref())
        .unwrap_or_default();
    let capped = char_prefix(answer, ANSWER_BINDING_CAP);
    (!capped.is_empty()).then(|| capped.to_string())
}
