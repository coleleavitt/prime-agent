//! Read-side views of a run: the `status()` report, the fused graph
//! snapshot (static machine structure plus the live overlay), the watch
//! signature, and the activity lane's wire conversion and frame cap.

use serde_json::{Map, Value, json};

use super::binding::{ANSWER_CAPTURE_CAP, char_prefix, py_json_dumps};
use super::model::{FactoryRun, RunState, StateEntry, StateRun, Status};
use crate::factory::spec::{
    MAX_TRANSITIONS_CAP,
    NODE_LIFECYCLE_DEFAULT,
    NODE_RETRIES_DEFAULT,
    RUN_FAILURE_POLICY_DEFAULT,
    RUN_MAX_CHILDREN_DEFAULT,
    RUN_MAX_PARALLEL_DEFAULT,
    STATE_MAX_ENTRIES_DEFAULT,
    TRANSITION_ON_KINDS,
};

/// Trailing ledger events `status()` returns.
pub const EVENT_WINDOW: usize = 200;
/// Trailing fired transitions a graph snapshot reports for edge marking.
pub const LAST_FIRED_WINDOW: usize = 10;
/// The unscoped graph's terminal-history window.
pub const GRAPH_RUNS_WINDOW: usize = 20;
/// Trailing ledger events a compact (host-lane) graph snapshot carries.
pub const GRAPH_EVENTS_TAIL: usize = 40;
/// Serialized byte cap on one activity reply frame.
pub const FACTORY_FRAME_CAP: usize = 262_144;

fn opt(value: Option<&str>) -> Value {
    value.map_or(Value::Null, Value::from)
}

/// One state's live report (the `status()` node shape; the graph reuses
/// it, without the answer preview on the compact lane). `running` and
/// `queued` are the stage's agent occupancy.
#[must_use]
pub fn state_report(state: &StateRun, include_answer: bool) -> Value {
    let instances = state.entries.iter().flat_map(|entry| {
        entry
            .instances
            .iter()
            .map(move |instance| (entry, instance))
    });
    let mut report = Map::new();
    report.insert("id".into(), Value::from(state.state_id.clone()));
    report.insert("status".into(), Value::from(state.status().as_str()));
    report.insert("lifecycle".into(), Value::from(state.lifecycle()));
    report.insert(
        "attempts".into(),
        Value::from(
            instances
                .clone()
                .map(|(_, instance)| u64::from(instance.attempt))
                .sum::<u64>(),
        ),
    );
    report.insert("entries_used".into(), Value::from(state.entries_used));
    report.insert("max_entries".into(), Value::from(state.max_entries));
    report.insert(
        "entries".into(),
        Value::Array(state.entries.iter().map(entry_report).collect()),
    );
    report.insert(
        "instances".into(),
        Value::Array(
            instances
                .clone()
                .map(|(entry, instance)| {
                    json!({
                        "index": instance.index,
                        "entry": entry.index,
                        "status": instance.status.as_str(),
                        "attempt": instance.attempt,
                        "child": instance.child_id,
                        "duration_ms": instance.duration_ms,
                        "error": instance.error,
                    })
                })
                .collect(),
        ),
    );
    report.insert(
        "running".into(),
        Value::from(
            instances
                .clone()
                .filter(|(_, instance)| instance.status == Status::Running)
                .count(),
        ),
    );
    report.insert(
        "queued".into(),
        Value::from(
            instances
                .filter(|(_, instance)| instance.status == Status::Pending)
                .count(),
        ),
    );
    if include_answer {
        if let Some(answer) = state
            .latest_settle()
            .and_then(|entry| entry.answer.as_deref())
            .filter(|answer| !answer.is_empty())
        {
            // The report previews compactly; the full binding text stays
            // on the entry.
            report.insert(
                "answer_preview".into(),
                Value::from(char_prefix(answer, ANSWER_CAPTURE_CAP)),
            );
        }
    }
    if let Some(error) = &state.error {
        report.insert("error".into(), Value::from(error.clone()));
    }
    if state.entries.iter().any(|entry| entry.needs_verify) {
        report.insert("needs_verify".into(), Value::Bool(true));
    }
    Value::Object(report)
}

/// One entry's report row: its status and error plus, for a needs-verify
/// entry, the provisional answer the child exit captured (compacted).
fn entry_report(entry: &StateEntry) -> Value {
    let mut row =
        json!({ "index": entry.index, "status": entry.status.as_str(), "error": entry.error });
    if entry.needs_verify {
        row["needs_verify"] = Value::Bool(true);
        if let Some(provisional) = entry
            .instances
            .iter()
            .rev()
            .filter(|instance| instance.provisional)
            .find_map(|instance| instance.answer.as_deref())
        {
            row["provisional_answer"] = Value::from(char_prefix(provisional, ANSWER_CAPTURE_CAP));
        }
    }
    row
}

/// The usage block `status()` returns (the graph reuses it).
#[must_use]
pub fn usage_report(run: &FactoryRun) -> Value {
    json!({
        "spawns": run.spawn_count,
        "settled": run.settle_count,
        "tool_uses": run.tool_use_total,
        "max_parallel": run.max_parallel,
        "max_children": run.max_children,
        "running": run.running_instance_count(),
        "transitions_fired": run.transitions_fired,
    })
}

/// The `status()` report. Marks the events the parent has not seen yet
/// (`recorded`/`arrived`) `delivered`; `shown` events keep their stage.
pub fn status_report(run: &mut FactoryRun, now: f64) -> Value {
    let nodes: Vec<Value> = run
        .states
        .iter()
        .map(|state| state_report(state, true))
        .collect();
    let mut marked = false;
    for event in &mut run.events {
        if matches!(
            event.get("stage").and_then(Value::as_str),
            Some("recorded" | "arrived")
        ) {
            event["stage"] = Value::from("delivered");
            marked = true;
        }
    }
    if marked {
        run.touch();
    }
    let start = run.events.len().saturating_sub(EVENT_WINDOW);
    let mut payload = json!({
        "run_id": run.run_id,
        "spec_id": run.spec_id,
        "name": run.name,
        "state": run.state.as_str(),
        "nodes": nodes,
        "events": run.events[start..].to_vec(),
        "elapsed_ms": elapsed_ms(run, now),
        "usage": usage_report(run),
    });
    // Every state whose entry carried a provisional answer (a child exit)
    // needs verification, root-visible without ledger archaeology.
    let needs_verify: Vec<Value> = run
        .states
        .iter()
        .filter(|state| state.entries.iter().any(|entry| entry.needs_verify))
        .map(|state| Value::from(state.state_id.clone()))
        .collect();
    if !needs_verify.is_empty() {
        payload["needs_verify"] = Value::Array(needs_verify);
    }
    // A paused run carries the last failed admission/bind error and a
    // one-line remedy (upstream #3462's M6).
    if run.state == RunState::Paused {
        payload["pause_reason"] = opt(run.pause_reason.as_deref());
        if let Some(last_error) = &run.last_error {
            payload["last_error"] = Value::from(last_error.clone());
            payload["remedy"] = Value::from(format!(
                "resume with await rlm.factory.resume('{}') to continue the remaining states; \
                 the failed state stays error - fix its subagent and start a fresh run to redo it",
                run.run_id
            ));
        }
    }
    payload
}

#[must_use]
pub fn elapsed_ms(run: &FactoryRun, now: f64) -> i64 {
    ((now - run.started_at) * 1000.0) as i64
}

/// `json.dumps(value, sort_keys=True)` identity text for edge keys.
fn sorted_text(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|key| (key.clone(), sorted(&map[key])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    py_json_dumps(&sorted(value))
}

/// The trailing fired transitions, newest first, at most
/// [`LAST_FIRED_WINDOW`]; an edge (from, to, guard) that fired twice keeps
/// its latest firing.
#[must_use]
pub fn last_fired(run: &FactoryRun) -> Vec<Value> {
    let mut latest: Vec<(String, Value)> = Vec::new();
    for event in &run.events {
        if event.get("kind").and_then(Value::as_str) != Some("transition_fired") {
            continue;
        }
        let from = event.get("from").cloned().unwrap_or(Value::Null);
        let when = event.get("when").cloned().unwrap_or(Value::Null);
        let to = event.get("to").cloned().unwrap_or(Value::Null);
        let key = format!(
            "{}->{}@{}",
            sorted_text(&from),
            to.as_str().unwrap_or_default(),
            sorted_text(&when)
        );
        let edge = json!({ "from": from, "to": to, "seq": event.get("seq"), "when": when });
        match latest.iter_mut().find(|(existing, _)| *existing == key) {
            Some(slot) => slot.1 = edge,
            None => latest.push((key, edge)),
        }
    }
    let mut ordered: Vec<Value> = latest.into_iter().map(|(_, edge)| edge).collect();
    ordered.sort_by_key(|edge| std::cmp::Reverse(edge["seq"].as_u64().unwrap_or(0)));
    ordered.truncate(LAST_FIRED_WINDOW);
    ordered
}

/// The trailing event window: the compact lane sheds `answer_captured`
/// rows and carries the shorter tail; graph is a pure read.
#[must_use]
pub fn graph_events(run: &FactoryRun, compact: bool) -> Vec<Value> {
    let window = if compact {
        GRAPH_EVENTS_TAIL
    } else {
        EVENT_WINDOW
    };
    let start = run.events.len().saturating_sub(window);
    run.events[start..]
        .iter()
        .filter(|event| {
            !compact || event.get("kind").and_then(Value::as_str) != Some("answer_captured")
        })
        .cloned()
        .collect()
}

/// One state's resolved spawn settings: `(state id, model, thinking)`.
pub type SpawnSettings = (String, Option<String>, Option<String>);

/// One canonical machine's static structure: the run block, each state's
/// declared shape (with resolved spawn settings for a live run), the
/// transitions with their guards, and the declared order.
#[must_use]
pub fn machine_structure(machine: &Value, resolved: Option<&[SpawnSettings]>) -> Value {
    let run_block = machine.get("run").filter(|run| run.is_object());
    let run_get = |key: &str| run_block.and_then(|run| run.get(key)).cloned();
    let mut run_out = Map::new();
    run_out.insert(
        "max_parallel".into(),
        run_get("max_parallel").unwrap_or(Value::from(RUN_MAX_PARALLEL_DEFAULT as u64)),
    );
    run_out.insert(
        "max_transitions".into(),
        run_get("max_transitions").unwrap_or(Value::from(MAX_TRANSITIONS_CAP as u64)),
    );
    run_out.insert(
        "failure_policy".into(),
        run_get("failure_policy").unwrap_or(Value::from(RUN_FAILURE_POLICY_DEFAULT)),
    );
    run_out.insert(
        "max_children".into(),
        run_get("max_children").unwrap_or(Value::from(RUN_MAX_CHILDREN_DEFAULT as u64)),
    );
    if let Some(budget) = run_get("budget_ms") {
        run_out.insert("budget_ms".into(), budget);
    }
    let empty = Vec::new();
    let states = machine
        .get("states")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut states_out = Vec::with_capacity(states.len());
    for state in states {
        let id = state.get("id").cloned().unwrap_or(Value::Null);
        let field = |key: &str, default: Value| state.get(key).cloned().unwrap_or(default);
        let mut row = Map::new();
        row.insert("id".into(), id.clone());
        row.insert(
            "entry".into(),
            Value::from(state.get("entry").and_then(Value::as_bool).unwrap_or(false)),
        );
        row.insert(
            "lifecycle".into(),
            field("lifecycle", Value::from(NODE_LIFECYCLE_DEFAULT)),
        );
        row.insert(
            "max_entries".into(),
            field("max_entries", Value::from(STATE_MAX_ENTRIES_DEFAULT as u64)),
        );
        row.insert(
            "retries".into(),
            field("retries", Value::from(NODE_RETRIES_DEFAULT as u64)),
        );
        let (mut model, mut thinking) = resolved
            .and_then(|resolved| {
                resolved
                    .iter()
                    .find(|(state_id, _, _)| Some(state_id.as_str()) == id.as_str())
            })
            .map_or((None, None), |(_, model, thinking)| {
                (
                    model.clone().map(Value::from),
                    thinking.clone().map(Value::from),
                )
            });
        let subagent = state.get("subagent").cloned().unwrap_or(Value::Null);
        if subagent.is_object() {
            if model.is_none() {
                model = subagent
                    .get("model")
                    .filter(|model| !model.is_null())
                    .cloned();
            }
            if thinking.is_none() {
                thinking = subagent
                    .get("thinking")
                    .filter(|thinking| !thinking.is_null())
                    .cloned();
            }
            if let Some(name) = subagent.get("name").filter(|name| json_truthy(name)) {
                row.insert("subagent".into(), name.clone());
            }
        } else {
            row.insert("subagent".into(), subagent);
        }
        if let Some(model) = model {
            row.insert("model".into(), model);
        }
        if let Some(thinking) = thinking {
            row.insert("thinking".into(), thinking);
        }
        states_out.push(Value::Object(row));
    }
    let transitions = machine
        .get("transitions")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let transitions_out: Vec<Value> = transitions
        .iter()
        .map(|transition| {
            let mut row = Map::new();
            row.insert(
                "from".into(),
                transition.get("from").cloned().unwrap_or(Value::Null),
            );
            row.insert(
                "to".into(),
                transition.get("to").cloned().unwrap_or(Value::Null),
            );
            row.insert(
                "on".into(),
                transition
                    .get("on")
                    .cloned()
                    .unwrap_or(Value::from(TRANSITION_ON_KINDS[0])),
            );
            if let Some(when) = transition.get("when") {
                row.insert("when".into(), when.clone());
            }
            Value::Object(row)
        })
        .collect();
    let order: Vec<Value> = states
        .iter()
        .map(|state| state.get("id").cloned().unwrap_or(Value::Null))
        .collect();
    json!({ "run": run_out, "states": states_out, "transitions": transitions_out, "order": order })
}

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

/// States with an entry or instance in flight (children-shaped activity).
fn active_nodes(run: &FactoryRun) -> Vec<String> {
    run.states
        .iter()
        .filter(|state| {
            state.entries.iter().any(|entry| {
                entry.status.in_flight()
                    || entry
                        .instances
                        .iter()
                        .any(|instance| instance.status.in_flight())
            })
        })
        .map(|state| state.state_id.clone())
        .collect()
}

/// One live run's fused snapshot.
#[must_use]
pub fn graph_snapshot(run: &FactoryRun, now: f64, compact: bool) -> Value {
    let elapsed = elapsed_ms(run, now);
    let resolved: Vec<SpawnSettings> = run
        .states
        .iter()
        .map(|state| {
            (
                state.state_id.clone(),
                state.model.clone(),
                state.thinking.clone(),
            )
        })
        .collect();
    json!({
        "run_id": run.run_id,
        "spec_id": run.spec_id,
        "name": run.name,
        "state": run.state.as_str(),
        "pause_reason": opt(run.pause_reason.as_deref()),
        "elapsed_ms": elapsed,
        "machine": machine_structure(&run.machine, Some(&resolved)),
        "nodes": run.states.iter().map(|state| state_report(state, !compact)).collect::<Vec<_>>(),
        "active_nodes": active_nodes(run),
        "last_fired": last_fired(run),
        "events": graph_events(run, compact),
        "usage": usage_report(run),
        "budget": { "limit_ms": run.run_budget_ms, "consumed_ms": elapsed },
    })
}

/// A stored spec's static graph: no live run, so the overlay is the shape
/// a fresh run starts from.
#[must_use]
pub fn spec_snapshot(spec_id: &str, canonical: &Value) -> Value {
    json!({
        "run_id": null,
        "spec_id": spec_id,
        "name": null,
        "state": null,
        "pause_reason": null,
        "elapsed_ms": 0,
        "machine": machine_structure(canonical, None),
        "nodes": [],
        "active_nodes": [],
        "last_fired": [],
        "events": [],
        "usage": null,
        "budget": { "limit_ms": canonical["run"].get("budget_ms").cloned().unwrap_or(Value::Null), "consumed_ms": 0 },
    })
}

/// The run's live shape a watch treats as a change: the run state, the
/// pause reason, the transition counter, and every state's entries and
/// instance statuses. Ledger-only movement re-arms the wait.
#[must_use]
pub fn signature(run: &FactoryRun) -> Value {
    json!([
        run.state.as_str(),
        run.pause_reason,
        run.transitions_fired,
        run.states
            .iter()
            .map(|state| json!([
                state.state_id,
                state.status().as_str(),
                state.entries_used,
                state
                    .entries
                    .iter()
                    .map(|entry| json!([
                        entry.index,
                        entry.status.as_str(),
                        entry
                            .instances
                            .iter()
                            .map(|instance| instance.status.as_str())
                            .collect::<Vec<_>>(),
                    ]))
                    .collect::<Vec<_>>(),
            ]))
            .collect::<Vec<_>>(),
    ])
}

/// `snake_case` -> the wire's camelCase (`run_id` -> `runId`; Python's
/// `str.capitalize` per later part).
fn wire_key(key: &str) -> String {
    let mut parts = key.split('_');
    let mut out = parts.next().unwrap_or_default().to_string();
    for part in parts {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(&chars.as_str().to_lowercase());
        }
    }
    out
}

/// The activity lane's wire conversion: dict keys re-key to camelCase
/// (values ride verbatim); a guard dict (`output` + `op`) rides verbatim
/// so the displayed condition matches the machine.
#[must_use]
pub fn wire_payload(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            if map.contains_key("output") && map.contains_key("op") {
                return value.clone();
            }
            Value::Object(
                map.iter()
                    .map(|(key, item)| (wire_key(key), wire_payload(item)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(wire_payload).collect()),
        other => other.clone(),
    }
}

/// Whether a wire run row is LIVE in the dock/page sense (a live state, or
/// children still in flight): the cap must never drop a live row.
fn wire_row_is_live(row: &Value) -> bool {
    if row
        .get("state")
        .and_then(Value::as_str)
        .is_some_and(|state| matches!(state, "running" | "stopping" | "paused"))
    {
        return true;
    }
    row.get("usage")
        .and_then(|usage| usage.get("running"))
        .and_then(Value::as_u64)
        .is_some_and(|running| running > 0)
}

/// One shed step for an all-runs reply: a row's event tail trims oldest
/// first (oldest row first), then the oldest droppable row drops; a live
/// row never drops and one row always stays.
fn shed_runs(runs: &mut Vec<Value>) -> bool {
    for row in runs.iter_mut() {
        if let Some(tail) = row.get_mut("events").and_then(Value::as_array_mut) {
            if tail.len() > 1 {
                tail.remove(0);
                return true;
            }
        }
    }
    if runs.len() > 1 {
        if let Some(index) = runs.iter().position(|row| !wire_row_is_live(row)) {
            runs.remove(index);
            return true;
        }
    }
    false
}

/// Keep one reply frame under [`FACTORY_FRAME_CAP`] (measured as the
/// Python JSON text the kernel lane used): the events tail trims oldest
/// first, an all-runs reply sheds per [`shed_runs`], and a frame that still
/// cannot fit fails loudly (a graph never truncates silently).
pub fn cap_factory_frame(frame: &mut Value) {
    while py_json_dumps(frame).len() > FACTORY_FRAME_CAP {
        let result = frame.get_mut("result");
        if let Some(events) = result
            .and_then(|result| result.get_mut("events"))
            .and_then(Value::as_array_mut)
        {
            if events.len() > 1 {
                events.remove(0);
                continue;
            }
        }
        if let Some(runs) = frame
            .get_mut("result")
            .and_then(|result| result.get_mut("runs"))
            .and_then(Value::as_array_mut)
        {
            if shed_runs(runs) {
                continue;
            }
        }
        if let Some(object) = frame.as_object_mut() {
            object.remove("result");
            object.insert("status".into(), Value::from("error"));
            object.insert(
                "reason".into(),
                Value::from("factory activity reply exceeds the wire cap"),
            );
        }
        return;
    }
}
