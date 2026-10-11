//! Factory spec validation, dag compilation, and canonicalization.
//!
//! A continual-harness `factory` entry stores a declarative state machine
//! of subagent states (`arguments["machine"]`): entry states (which declare
//! no inputs), guarded transitions between states, and bounded re-entry
//! (`max_entries`). The original dag form (`arguments["dag"]`) stays as
//! sugar: it compiles to machine form (each node becomes a state entered
//! once; a node's full effective dependency set compiles to ONE join
//! transition that waits for every predecessor). Wait states are gated: a
//! state carrying a `wait` block is rejected at write time until the watch
//! host handlers exist.
//!
//! This is the single implementation of the write-time dry run: the kernel's
//! `rlm.factory` validator functions are thin clients over it, and the
//! executor canonicalizes through it at run time. Every rule and error
//! sentence matches the original Python validator byte for byte (values are
//! [`PyValue`]s, and `{value!r}` renders through [`py_repr`]).

use std::collections::{BinaryHeap, HashMap, HashSet};

use super::labels::{SUBAGENT_NAME_MAX_LENGTH, suffixed_spawn_form};
use super::pyvalue::{PyValue, py_repr, py_str_repr, py_strip};

pub const FAILURE_POLICIES: [&str; 3] = ["fail_fast", "continue", "escalate"];
pub const PORT_TYPES: [&str; 2] = ["text", "json"];
pub const LIFECYCLES: [&str; 2] = ["task", "resident"];
pub const TRANSITION_ON_KINDS: [&str; 1] = ["settled"];
pub const GUARD_OPS: [&str; 8] = ["eq", "ne", "gt", "gte", "lt", "lte", "exists", "contains"];
pub const MAX_NODES: usize = 1024;
pub const MAX_STATES: usize = MAX_NODES;
pub const MAX_RETRIES: i128 = 10;
pub const MAX_PARALLEL_MIN: i128 = 1;
pub const MAX_PARALLEL_MAX: i128 = 64;
pub const FOREACH_MAX_MIN: i128 = 1;
pub const FOREACH_MAX_MAX: i128 = 256;
pub const MAX_TRANSITIONS_CAP: i128 = 10_000;
pub const MAX_CHILDREN_CAP: i128 = 1_000_000;
pub const TRANSITIONS_PER_STATE_DEFAULT: i128 = 10;
pub const RUN_FAILURE_POLICY_DEFAULT: &str = "escalate";
pub const RUN_MAX_PARALLEL_DEFAULT: i128 = 8;
pub const RUN_MAX_CHILDREN_DEFAULT: i128 = 10_000;
pub const NODE_LIFECYCLE_DEFAULT: &str = "task";
pub const NODE_RETRIES_DEFAULT: i128 = 0;
pub const STATE_MAX_ENTRIES_DEFAULT: i128 = 1;
/// Nesting bound on one guard comparison value (`when.value`): every seam
/// the value rides recurses per level, so a deeper container is rejected
/// by the finite-JSON-data rule.
pub const MAX_GUARD_VALUE_DEPTH: usize = 256;

/// `repr(list(<tuple of strings>))`, e.g. `['task', 'resident']`.
fn str_list_repr(items: &[&str]) -> String {
    let rendered: Vec<String> = items.iter().map(|item| py_str_repr(item)).collect();
    format!("[{}]", rendered.join(", "))
}

/// `value in <tuple of strings>`: only a str can be a member.
fn str_member(value: &PyValue, members: &[&str]) -> bool {
    value.as_str().is_some_and(|text| members.contains(&text))
}

/// `_valid_node_id`: `^[a-z0-9][a-z0-9-]{0,63}$`.
fn valid_node_id(value: &PyValue) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    let bytes = text.as_bytes();
    let slug = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    !bytes.is_empty()
        && bytes.len() <= 64
        && slug(&bytes[0])
        && bytes[1..].iter().all(|byte| slug(byte) || *byte == b'-')
}

/// `_port_list`: the node's inputs/outputs list, or `[]` when absent or
/// malformed.
fn port_list<'a>(node: &'a PyValue, key: &str) -> &'a [PyValue] {
    node.get(key).as_list().unwrap_or(&[])
}

/// `_declared_port_types`: port name to type for the well-formed entries.
fn declared_port_types(node: &PyValue, key: &str) -> HashMap<String, String> {
    let mut ports = HashMap::new();
    for entry in port_list(node, key) {
        if entry.is_dict() {
            if let (Some(name), Some(port_type)) =
                (entry.get("name").as_str(), entry.get("type").as_str())
            {
                if !name.is_empty() && PORT_TYPES.contains(&port_type) {
                    ports.insert(name.to_string(), port_type.to_string());
                }
            }
        }
    }
    ports
}

/// `str.partition(".")`: `(head, tail)` around the first dot.
fn partition_dot(text: &str) -> (&str, &str) {
    text.split_once('.').unwrap_or((text, ""))
}

/// `_input_sources`: the source node ids the node's inputs reference.
fn input_sources(node: &PyValue) -> Vec<String> {
    let mut sources = Vec::new();
    for input in port_list(node, "inputs") {
        if !input.is_dict() {
            continue;
        }
        if let Some(source) = input.get("from").as_str() {
            if source.contains('.') {
                sources.push(partition_dot(source).0.to_string());
            }
        }
    }
    sources
}

/// `_is_machine_form`: a states/transitions key wins.
fn is_machine_form(spec: &PyValue) -> bool {
    spec.is_dict() && (spec.has("states") || spec.has("transitions"))
}

/// The lifecycle a state reads as (`state.get("lifecycle", "task")`).
fn lifecycle_of(state: &PyValue) -> &PyValue {
    static TASK: std::sync::OnceLock<PyValue> = std::sync::OnceLock::new();
    state
        .entry("lifecycle")
        .unwrap_or_else(|| TASK.get_or_init(|| PyValue::Str(NODE_LIFECYCLE_DEFAULT.to_string())))
}

fn is_resident(state: &PyValue) -> bool {
    lifecycle_of(state).as_str() == Some("resident")
}

/// `_value_is_finite`: JSON-clean all the way down (finite floats, string
/// keys, JSON shapes only), acyclic (cycles arrive as opaque leaves), and
/// nested no deeper than [`MAX_GUARD_VALUE_DEPTH`].
fn value_is_finite(value: &PyValue, depth: usize) -> bool {
    if depth > MAX_GUARD_VALUE_DEPTH {
        return false;
    }
    match value {
        PyValue::Float(number) => number.is_finite(),
        PyValue::None
        | PyValue::Bool(_)
        | PyValue::Int(_)
        | PyValue::BigInt(_)
        | PyValue::Str(_) => true,
        PyValue::List(items) => items.iter().all(|item| value_is_finite(item, depth + 1)),
        PyValue::Dict(pairs) => pairs
            .iter()
            .all(|(key, item)| matches!(key, PyValue::Str(_)) && value_is_finite(item, depth + 1)),
        PyValue::Opaque { .. } => false,
    }
}

// ---------------------------------------------------------------------------
// Shared field checks (the dag compiler and the machine validator).
// ---------------------------------------------------------------------------

/// `_validate_run_fields`: the run budget when valid, else `None`. Typed
/// fields validate by PRESENCE: an explicit null is rejected with the
/// field's own message.
fn validate_run_fields<'a>(
    run: Option<&'a PyValue>,
    errors: &mut Vec<String>,
) -> Option<&'a PyValue> {
    let run = run.filter(|run| run.is_dict())?;
    if run.has("budget_ms") && !run.get("budget_ms").is_positive_int() {
        errors.push("run budget_ms must be a positive integer".to_string());
    }
    let run_budget = Some(run.get("budget_ms")).filter(|budget| budget.is_positive_int());
    if run.has("failure_policy") && !str_member(run.get("failure_policy"), &FAILURE_POLICIES) {
        errors.push(format!(
            "run failure_policy must be one of {}, got {}",
            str_list_repr(&FAILURE_POLICIES),
            py_repr(run.get("failure_policy"))
        ));
    }
    let max_parallel = run.get("max_parallel");
    if run.has("max_parallel")
        && !(max_parallel.is_int()
            && max_parallel.int_ge(MAX_PARALLEL_MIN)
            && max_parallel.int_le(MAX_PARALLEL_MAX))
    {
        errors.push(format!(
            "run max_parallel must be an integer between {MAX_PARALLEL_MIN} and {MAX_PARALLEL_MAX}"
        ));
    }
    let max_transitions = run.get("max_transitions");
    if run.has("max_transitions")
        && !(max_transitions.is_positive_int() && max_transitions.int_le(MAX_TRANSITIONS_CAP))
    {
        errors.push(format!(
            "run max_transitions must be a positive integer no greater than {MAX_TRANSITIONS_CAP}"
        ));
    }
    let max_children = run.get("max_children");
    if run.has("max_children")
        && !(max_children.is_positive_int() && max_children.int_le(MAX_CHILDREN_CAP))
    {
        errors.push(format!(
            "run max_children must be a positive integer no greater than {MAX_CHILDREN_CAP}"
        ));
    }
    run_budget
}

/// One machine's states (or one dag's nodes) by id, in insertion order.
struct StatesById<'a> {
    order: Vec<&'a str>,
    by_id: HashMap<&'a str, &'a PyValue>,
}

impl<'a> StatesById<'a> {
    fn new() -> Self {
        Self {
            order: Vec::new(),
            by_id: HashMap::new(),
        }
    }

    fn insert(&mut self, id: &'a str, state: &'a PyValue) {
        self.order.push(id);
        self.by_id.insert(id, state);
    }

    fn get(&self, id: &str) -> Option<&'a PyValue> {
        self.by_id.get(id).copied()
    }

    fn contains(&self, id: &str) -> bool {
        self.by_id.contains_key(id)
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = (&'a str, &'a PyValue)> + '_ {
        self.order.iter().map(|id| (*id, self.by_id[id]))
    }
}

/// `_validate_state_fields`: the field rules shared by dag nodes
/// (`noun = "node"`) and machine states (`noun = "state"`).
fn validate_state_fields(
    state: &PyValue,
    run_budget: Option<&PyValue>,
    states_by_id: &StatesById<'_>,
    noun: &str,
    errors: &mut Vec<String>,
) {
    let reference = state.get("id").as_str().unwrap_or_default();
    let lifecycle = lifecycle_of(state);
    if state.has("lifecycle") && !str_member(lifecycle, &LIFECYCLES) {
        errors.push(format!(
            "{noun} {reference} lifecycle must be 'task' or 'resident', got {}",
            py_repr(lifecycle)
        ));
    }
    let resident = lifecycle.as_str() == Some("resident");

    if !state.get("wait").is_none() {
        errors.push(format!(
            "{noun} {reference}: wait states require the watch host handlers (rlm.watch.*); \
             they arrive with the communication series - remove the wait block until then"
        ));
    }

    let subagent = state.get("subagent");
    if subagent.is_nonempty_str() {
        // A harness subagent entry id or title; resolved at run time.
    } else if subagent.is_dict() {
        let prompt_ok = subagent
            .get("prompt")
            .as_str()
            .is_some_and(|prompt| !py_strip(prompt).is_empty());
        if !prompt_ok {
            errors.push(format!(
                "{noun} {reference} inline subagent requires a non-empty prompt"
            ));
        }
        for key in ["name", "model", "thinking"] {
            let value = subagent.get(key);
            let valid = match value {
                PyValue::None => true,
                PyValue::Str(text) => !py_strip(text).is_empty(),
                _ => false,
            };
            if !valid {
                errors.push(format!(
                    "{noun} {reference} inline subagent {key} must be a non-empty string when provided"
                ));
            }
        }
        if let Some(configured) = subagent.get("name").as_str() {
            let length = py_strip(configured).chars().count();
            if length > SUBAGENT_NAME_MAX_LENGTH {
                errors.push(format!(
                    "{noun} {reference} inline subagent name must be at most \
                     {SUBAGENT_NAME_MAX_LENGTH} characters, got {length}"
                ));
            }
        }
    } else {
        errors.push(format!(
            "{noun} {reference} requires a subagent: a harness subagent id/title string \
             or an inline object with a prompt"
        ));
    }

    if state.has("budget_ms") {
        let budget = state.get("budget_ms");
        if !budget.is_positive_int() {
            errors.push(format!(
                "{noun} {reference} budget_ms must be a positive integer"
            ));
        } else if let Some(run_budget) = run_budget {
            if budget.int_greater_than(run_budget) {
                errors.push(format!(
                    "{noun} {reference} budget_ms {} exceeds the run budget_ms {}",
                    py_repr(budget),
                    py_repr(run_budget)
                ));
            }
        }
    }

    if state.has("retries") {
        let retries = state.get("retries");
        if !(retries.is_int() && retries.int_ge(0) && retries.int_le(MAX_RETRIES)) {
            errors.push(format!(
                "{noun} {reference} retries must be an integer between 0 and {MAX_RETRIES}"
            ));
        }
    }

    if state.has("failure_policy") && !str_member(state.get("failure_policy"), &FAILURE_POLICIES) {
        errors.push(format!(
            "{noun} {reference} failure_policy must be one of {}, got {}",
            str_list_repr(&FAILURE_POLICIES),
            py_repr(state.get("failure_policy"))
        ));
    }

    let outputs = state.get("outputs");
    if !outputs.is_none() && !outputs.is_list() {
        errors.push(format!("{noun} {reference} outputs must be a list"));
    } else if resident && outputs.as_list().is_some_and(|items| !items.is_empty()) {
        errors.push(format!(
            "resident {noun} {reference} cannot declare outputs"
        ));
    }
    let mut seen_outputs: HashSet<String> = HashSet::new();
    let mut reported_outputs: HashSet<String> = HashSet::new();
    for (index, out) in port_list(state, "outputs").iter().enumerate() {
        if !out.is_dict() {
            errors.push(format!(
                "{noun} {reference} outputs[{index}] must be an object"
            ));
            continue;
        }
        let name = out.get("name");
        match name.as_str() {
            Some(text) if !text.is_empty() => {
                if seen_outputs.contains(text) {
                    if reported_outputs.insert(text.to_string()) {
                        errors.push(format!(
                            "{noun} {reference} declares duplicate output name {}",
                            py_repr(name)
                        ));
                    }
                } else {
                    seen_outputs.insert(text.to_string());
                }
            }
            _ => errors.push(format!(
                "{noun} {reference} outputs[{index}] requires a non-empty name"
            )),
        }
        if !str_member(out.get("type"), &PORT_TYPES) {
            errors.push(format!(
                "{noun} {reference} output {} type must be 'text' or 'json'",
                py_repr(name)
            ));
        }
    }

    let inputs = state.get("inputs");
    if !inputs.is_none() && !inputs.is_list() {
        errors.push(format!("{noun} {reference} inputs must be a list"));
    }
    let mut seen_inputs: HashSet<String> = HashSet::new();
    let mut reported_inputs: HashSet<String> = HashSet::new();
    let mut output_types_by_source: HashMap<String, HashMap<String, String>> = HashMap::new();
    for (index, input) in port_list(state, "inputs").iter().enumerate() {
        if !input.is_dict() {
            errors.push(format!(
                "{noun} {reference} inputs[{index}] must be an object"
            ));
            continue;
        }
        let name = input.get("name");
        let port_type = input.get("type");
        let source = input.get("from");
        let optional = input.get("optional");
        if !optional.is_none() && !matches!(optional, PyValue::Bool(_)) {
            errors.push(format!(
                "{noun} {reference} input {} optional must be a boolean when provided",
                py_repr(name)
            ));
        }
        match name.as_str() {
            Some(text) if !text.is_empty() => {
                if seen_inputs.contains(text) {
                    if reported_inputs.insert(text.to_string()) {
                        errors.push(format!(
                            "{noun} {reference} declares duplicate input name {}",
                            py_repr(name)
                        ));
                    }
                } else {
                    seen_inputs.insert(text.to_string());
                }
            }
            _ => errors.push(format!(
                "{noun} {reference} inputs[{index}] requires a non-empty name"
            )),
        }
        let type_known = str_member(port_type, &PORT_TYPES);
        if !type_known {
            errors.push(format!(
                "{noun} {reference} input {} type must be 'text' or 'json'",
                py_repr(name)
            ));
        }
        let Some(source) = source.as_str().filter(|source| source.contains('.')) else {
            errors.push(format!(
                "{noun} {reference} input {} requires a 'from' reference of the form '<node_id>.<output_name>'",
                py_repr(name)
            ));
            continue;
        };
        let (src_id, src_output) = partition_dot(source);
        let Some(src) = states_by_id.get(src_id) else {
            errors.push(format!(
                "{noun} {reference} input {} references unknown {noun} {}",
                py_repr(name),
                py_str_repr(src_id)
            ));
            continue;
        };
        if is_resident(src) {
            errors.push(format!(
                "{noun} {reference} input {} cannot read from resident {noun} {}",
                py_repr(name),
                py_str_repr(src_id)
            ));
            continue;
        }
        let src_types = output_types_by_source
            .entry(src_id.to_string())
            .or_insert_with(|| declared_port_types(src, "outputs"));
        match src_types.get(src_output) {
            None => errors.push(format!(
                "{noun} {reference} input {} references output {} that {noun} {} does not declare",
                py_repr(name),
                py_str_repr(src_output),
                py_str_repr(src_id)
            )),
            Some(src_type) if type_known && Some(src_type.as_str()) != port_type.as_str() => {
                errors.push(format!(
                    "{noun} {reference} input {} of type {} cannot read from output {} of type {}",
                    py_repr(name),
                    py_repr(port_type),
                    py_str_repr(src_output),
                    py_str_repr(src_type)
                ));
            }
            Some(_) => {}
        }
    }

    let foreach = state.get("foreach");
    if !foreach.is_none() {
        if resident {
            errors.push(format!("resident {noun} {reference} cannot use foreach"));
        }
        if foreach.is_dict() {
            let over = foreach.get("over");
            match over.as_str() {
                Some(over_name) if !over_name.is_empty() => {
                    let declared = declared_port_types(state, "inputs");
                    match declared.get(over_name) {
                        None => errors.push(format!(
                            "{noun} {reference} foreach.over must name one of this {noun}'s inputs, got {}",
                            py_repr(over)
                        )),
                        Some(port_type) if port_type != "json" => errors.push(format!(
                            "{noun} {reference} foreach.over input {} must have type 'json'",
                            py_repr(over)
                        )),
                        Some(_) => {}
                    }
                }
                _ => errors.push(format!(
                    "{noun} {reference} foreach.over must be a non-empty input name"
                )),
            }
            let foreach_max = foreach.get("max");
            if !(foreach_max.is_int()
                && foreach_max.int_ge(FOREACH_MAX_MIN)
                && foreach_max.int_le(FOREACH_MAX_MAX))
            {
                errors.push(format!(
                    "{noun} {reference} foreach.max must be an integer between {FOREACH_MAX_MIN} and {FOREACH_MAX_MAX}"
                ));
            }
        } else {
            errors.push(format!("{noun} {reference} foreach must be an object"));
        }
    }
}

// ---------------------------------------------------------------------------
// Machine-form validation.
// ---------------------------------------------------------------------------

/// `_validate_guard`: one transition guard over the from-state's outputs.
fn validate_guard(when: &PyValue, index: usize, src_state: &PyValue, errors: &mut Vec<String>) {
    if !when.is_dict() {
        errors.push(format!("transitions[{index}] when must be an object"));
        return;
    }
    let output = when.get("output");
    let src_types = declared_port_types(src_state, "outputs");
    match output.as_str() {
        Some(port) if !port.is_empty() => match src_types.get(port) {
            None => errors.push(format!(
                "transitions[{index}] when.output {} is not a declared output of state {}",
                py_repr(output),
                py_repr(src_state.get("id"))
            )),
            Some(port_type) => {
                let path = when.get("path");
                if !path.is_none() {
                    if !path.is_nonempty_str() {
                        errors.push(format!(
                            "transitions[{index}] when.path must be a non-empty dotted path"
                        ));
                    } else if port_type != "json" {
                        errors.push(format!(
                            "transitions[{index}] when.path requires a json output, got text output {}",
                            py_repr(output)
                        ));
                    }
                }
            }
        },
        _ => errors.push(format!(
            "transitions[{index}] when requires a non-empty output"
        )),
    }
    let op = when.get("op");
    if !str_member(op, &GUARD_OPS) {
        errors.push(format!(
            "transitions[{index}] when.op must be one of {}, got {}",
            str_list_repr(&GUARD_OPS),
            py_repr(op)
        ));
        return;
    }
    let op = op.as_str().unwrap_or_default();
    if op == "exists" {
        return;
    }
    let value = when.get("value");
    match op {
        "gt" | "gte" | "lt" | "lte" => {
            if !value.is_number() {
                errors.push(format!(
                    "transitions[{index}] when.op {} requires a numeric value",
                    py_str_repr(op)
                ));
            }
        }
        "contains" => {
            if value.as_list().is_none_or(<[PyValue]>::is_empty) {
                errors.push(format!(
                    "transitions[{index}] when.op 'contains' requires a non-empty list value"
                ));
            }
        }
        _ => {
            if !value.is_scalar() {
                errors.push(format!(
                    "transitions[{index}] when.op {} requires a scalar value",
                    py_str_repr(op)
                ));
            }
        }
    }
    if !value_is_finite(value, 0) {
        errors.push(format!(
            "transitions[{index}] when.value must be finite JSON data \
             (JSON carries no NaN or Infinity, and only JSON shapes \
             serialize: lists, objects, strings, numbers, booleans, null, \
             and no container nests deeper than {MAX_GUARD_VALUE_DEPTH} levels)"
        ));
    }
}

/// Dry-run validation for a machine-form factory spec: the error sentences
/// (empty when valid). States are 1..1024 with unique slug ids and at least
/// one entry state; entry states declare no inputs; resident states declare
/// no outputs, foreach, or outgoing transitions; transitions reference
/// existing states (self-loops are legal re-entry) and may carry one guard
/// over the from-state's latest settle output; a list `from` is a join
/// (guards are single-source only). Cycles are legal.
#[must_use]
pub fn validate_factory_machine(machine: &PyValue) -> Vec<String> {
    if !machine.is_dict() {
        return vec!["factory machine must be a JSON object".to_string()];
    }
    let mut errors = Vec::new();
    let mut run = machine.entry("run").filter(|run| !run.is_none());
    if run.is_some_and(|run| !run.is_dict()) {
        errors.push("run must be an object".to_string());
        run = None;
    }
    let run_budget = validate_run_fields(run, &mut errors);

    let Some(states) = machine.get("states").as_list() else {
        errors.push("factory machine requires a states list".to_string());
        return errors;
    };
    if !(1..=MAX_STATES).contains(&states.len()) {
        errors.push(format!(
            "factory machine must declare between 1 and {MAX_STATES} states, got {}",
            states.len()
        ));
        return errors;
    }

    let mut states_by_id = StatesById::new();
    for (index, state) in states.iter().enumerate() {
        if !state.is_dict() {
            errors.push(format!("states[{index}] must be an object"));
            continue;
        }
        let state_id = state.get("id");
        if !state_id.is_nonempty_str() {
            errors.push(format!("states[{index}] requires a non-empty id"));
        } else if !valid_node_id(state_id) {
            errors.push(format!(
                "states[{index}] id must match ^[a-z0-9][a-z0-9-]{{0,63}}$, got {}",
                py_repr(state_id)
            ));
        } else {
            let id = state_id.as_str().unwrap_or_default();
            if states_by_id.contains(id) {
                errors.push(format!(
                    "states[{index}] duplicates state id {}",
                    py_repr(state_id)
                ));
            } else {
                states_by_id.insert(id, state);
            }
        }
    }

    // Configured inline names label the spawned children verbatim, so two
    // states sharing one name (or one state's name shadowing another's
    // suffixed labels) would collide on the supervisor's unique sibling
    // names at spawn time.
    let mut seen_names: Vec<(String, String)> = Vec::new();
    for (state_id, state) in states_by_id.iter() {
        validate_state_fields(state, run_budget, &states_by_id, "state", &mut errors);
        let subagent = state.get("subagent");
        if subagent.is_dict() {
            if let Some(name) = subagent
                .get("name")
                .as_str()
                .filter(|name| !name.is_empty())
            {
                let configured = py_strip(name).to_string();
                let state_of = |seen: &str| {
                    seen_names
                        .iter()
                        .find(|(name, _)| name == seen)
                        .map(|(_, state)| state.clone())
                        .unwrap_or_default()
                };
                let base_seen = seen_names
                    .iter()
                    .map(|(seen, _)| seen.clone())
                    .find(|seen| suffixed_spawn_form(seen, &configured));
                let base_current = if base_seen.is_some() {
                    None
                } else {
                    seen_names
                        .iter()
                        .map(|(seen, _)| seen.clone())
                        .find(|seen| suffixed_spawn_form(&configured, seen))
                };
                if seen_names.iter().any(|(seen, _)| *seen == configured) {
                    errors.push(format!(
                        "state {state_id} subagent name {} is already configured by state {}",
                        py_str_repr(&configured),
                        py_str_repr(&state_of(&configured))
                    ));
                } else if let Some(base) = base_seen {
                    errors.push(format!(
                        "state {state_id} subagent name {} collides with the suffixed spawn labels \
                         of state {} (configured {}): re-entry, foreach, and retries name children \
                         {}-i<n> and {}-a<n>",
                        py_str_repr(&configured),
                        py_str_repr(&state_of(&base)),
                        py_str_repr(&base),
                        py_str_repr(&base),
                        py_str_repr(&base)
                    ));
                } else if let Some(base) = base_current {
                    errors.push(format!(
                        "state {state_id} subagent name {} suffixed by re-entry, foreach, and retries \
                         ({}-i<n>, {}-a<n>) collides with state {} (configured {})",
                        py_str_repr(&configured),
                        py_str_repr(&configured),
                        py_str_repr(&configured),
                        py_str_repr(&state_of(&base)),
                        py_str_repr(&base)
                    ));
                } else {
                    seen_names.push((configured, state_id.to_string()));
                }
            }
        }
        if state.has("entry") && !matches!(state.get("entry"), PyValue::Bool(_)) {
            errors.push(format!("state {state_id} entry must be a boolean"));
        }
        if state.has("max_entries") {
            let max_entries = state.get("max_entries");
            if !(max_entries.is_int() && max_entries.int_ge(STATE_MAX_ENTRIES_DEFAULT)) {
                errors.push(format!(
                    "state {state_id} max_entries must be an integer >= {STATE_MAX_ENTRIES_DEFAULT}"
                ));
            }
        }
        if state.get("entry").is_true() && !port_list(state, "inputs").is_empty() {
            errors.push(format!("entry state {state_id} cannot declare inputs"));
        }
        // A REQUIRED self-input can never bind on the state's first entry;
        // the optional variant is the loop form.
        for input in port_list(state, "inputs") {
            if !input.is_dict() || input.get("optional").truthy() {
                continue;
            }
            if let Some(source) = input.get("from").as_str() {
                if source.contains('.') && partition_dot(source).0 == state_id {
                    errors.push(format!(
                        "state {state_id} input {} cannot require itself: mark the self-input \
                         optional - a required one can never bind on the state's first entry",
                        py_repr(input.get("name"))
                    ));
                }
            }
        }
    }

    if !states_by_id.is_empty()
        && !states_by_id
            .iter()
            .any(|(_, state)| state.get("entry").is_true())
    {
        errors.push("factory machine requires at least one entry state".to_string());
    }

    let transitions = match machine.get("transitions") {
        PyValue::None => &[][..],
        PyValue::List(items) => items.as_slice(),
        _ => {
            errors.push("factory machine transitions must be a list".to_string());
            return errors;
        }
    };
    for (index, transition) in transitions.iter().enumerate() {
        if !transition.is_dict() {
            errors.push(format!("transitions[{index}] must be an object"));
            continue;
        }
        let raw_src = transition.get("from");
        let dst = transition.get("to");
        let sources: Vec<&PyValue> = if let Some(list) = raw_src.as_list() {
            if list.is_empty() {
                errors.push(format!(
                    "transitions[{index}] from must name at least one state"
                ));
            } else if !list.iter().all(PyValue::is_nonempty_str) {
                errors.push(format!(
                    "transitions[{index}] from entries must be non-empty state id strings"
                ));
            } else {
                let unique: HashSet<&str> = list.iter().filter_map(PyValue::as_str).collect();
                if unique.len() != list.len() {
                    errors.push(format!("transitions[{index}] from must not repeat a state"));
                } else if let Some(missing) = list
                    .iter()
                    .find(|src| !states_by_id.contains(src.as_str().unwrap_or_default()))
                {
                    errors.push(format!(
                        "transitions[{index}] references unknown from-state {}",
                        py_repr(missing)
                    ));
                }
            }
            if !transition.get("when").is_none() {
                errors.push(format!(
                    "transitions[{index}] with multiple from-states cannot carry a when guard; \
                     use single-state transitions for guards"
                ));
            }
            list.iter().collect()
        } else if raw_src.is_nonempty_str() {
            if !states_by_id.contains(raw_src.as_str().unwrap_or_default()) {
                errors.push(format!(
                    "transitions[{index}] references unknown from-state {}",
                    py_repr(raw_src)
                ));
            }
            vec![raw_src]
        } else {
            errors.push(format!("transitions[{index}] requires a non-empty from"));
            Vec::new()
        };
        if !dst.is_nonempty_str() {
            errors.push(format!("transitions[{index}] requires a non-empty to"));
        } else if !states_by_id.contains(dst.as_str().unwrap_or_default()) {
            errors.push(format!(
                "transitions[{index}] references unknown to-state {}",
                py_repr(dst)
            ));
        }
        if let Some(on) = transition.entry("on") {
            if !str_member(on, &TRANSITION_ON_KINDS) {
                errors.push(format!(
                    "transitions[{index}] on must be one of {}, got {}",
                    str_list_repr(&TRANSITION_ON_KINDS),
                    py_repr(on)
                ));
            }
        }
        for src in &sources {
            let Some(src_state) = src
                .as_str()
                .filter(|id| !id.is_empty())
                .and_then(|id| states_by_id.get(id))
            else {
                continue;
            };
            if is_resident(src_state) {
                errors.push(format!(
                    "transitions[{index}] cannot leave resident state {}",
                    py_repr(src)
                ));
            }
            if sources.len() == 1 {
                let when = transition.get("when");
                if !when.is_none() {
                    validate_guard(when, index, src_state, &mut errors);
                }
            }
        }
    }
    errors
}

// ---------------------------------------------------------------------------
// Dag compatibility: compile the V1 dag form to machine form.
// ---------------------------------------------------------------------------

/// `_effective_dag_edges`: `depends_on` plus every `inputs[].from` source,
/// deduplicated in first-seen order.
fn effective_dag_edges(node: &PyValue) -> Vec<String> {
    let mut edges: Vec<String> = Vec::new();
    for dep in port_list(node, "depends_on") {
        if let Some(dep) = dep.as_str() {
            if !dep.is_empty() && !edges.iter().any(|edge| edge == dep) {
                edges.push(dep.to_string());
            }
        }
    }
    for source in input_sources(node) {
        if !edges.contains(&source) {
            edges.push(source);
        }
    }
    edges
}

fn dict(pairs: Vec<(&str, PyValue)>) -> PyValue {
    PyValue::Dict(
        pairs
            .into_iter()
            .map(|(key, value)| (PyValue::Str(key.to_string()), value))
            .collect(),
    )
}

/// Compile a dag-form spec into machine form: `(Some(machine), [])` on
/// success (defaults are applied later by [`canonicalize_factory_spec`]),
/// `(None, errors)` with the V1 dag wording otherwise. Each node becomes a
/// state (`entry` when it has no effective dependencies, `max_entries` 1);
/// the node's full dependency set becomes ONE guard-less join transition
/// (one dependency stays a plain `from` string).
#[must_use]
pub fn compile_factory_dag(dag: &PyValue) -> (Option<PyValue>, Vec<String>) {
    if !dag.is_dict() {
        return (None, vec!["factory dag must be a JSON object".to_string()]);
    }
    let mut errors = Vec::new();
    let mut run = dag.entry("run").filter(|run| !run.is_none());
    if run.is_some_and(|run| !run.is_dict()) {
        errors.push("run must be an object".to_string());
        run = None;
    }
    let run_budget = validate_run_fields(run, &mut errors);

    let Some(nodes) = dag.get("nodes").as_list() else {
        errors.push("factory dag requires a nodes list".to_string());
        return (None, errors);
    };
    if !(1..=MAX_NODES).contains(&nodes.len()) {
        errors.push(format!(
            "factory dag must declare between 1 and {MAX_NODES} nodes, got {}",
            nodes.len()
        ));
        return (None, errors);
    }

    let mut nodes_by_id = StatesById::new();
    for (index, node) in nodes.iter().enumerate() {
        if !node.is_dict() {
            errors.push(format!("nodes[{index}] must be an object"));
            continue;
        }
        let node_id = node.get("id");
        if !node_id.is_nonempty_str() {
            errors.push(format!("nodes[{index}] requires a non-empty id"));
        } else if !valid_node_id(node_id) {
            errors.push(format!(
                "nodes[{index}] id must match ^[a-z0-9][a-z0-9-]{{0,63}}$, got {}",
                py_repr(node_id)
            ));
        } else {
            let id = node_id.as_str().unwrap_or_default();
            if nodes_by_id.contains(id) {
                errors.push(format!(
                    "nodes[{index}] duplicates node id {}",
                    py_repr(node_id)
                ));
            } else {
                nodes_by_id.insert(id, node);
            }
        }
    }

    for (node_id, node) in nodes_by_id.iter() {
        validate_state_fields(node, run_budget, &nodes_by_id, "node", &mut errors);
        if input_sources(node).iter().any(|source| source == node_id) {
            errors.push(format!("node {node_id} cannot depend on itself"));
        }
        let depends_on = node.get("depends_on");
        if !depends_on.is_none() {
            match depends_on.as_list() {
                None => errors.push(format!(
                    "node {node_id} depends_on must be a list of node ids"
                )),
                Some(deps) => {
                    for dep in deps {
                        match dep.as_str().filter(|dep| !dep.is_empty()) {
                            None => errors.push(format!(
                                "node {node_id} depends_on entries must be non-empty node id strings"
                            )),
                            Some(dep) if dep == node_id => {
                                errors.push(format!("node {node_id} cannot depend on itself"));
                            }
                            Some(dep) => match nodes_by_id.get(dep) {
                                None => errors.push(format!(
                                    "node {node_id} depends on unknown node {}",
                                    py_str_repr(dep)
                                )),
                                Some(target) if is_resident(target) => errors.push(format!(
                                    "node {node_id} cannot depend on resident node {}",
                                    py_str_repr(dep)
                                )),
                                Some(_) => {}
                            },
                        }
                    }
                }
            }
        }
    }
    if !errors.is_empty() {
        return (None, errors);
    }

    let mut states = Vec::with_capacity(nodes.len());
    let mut transitions = Vec::new();
    for node in nodes {
        let edges = effective_dag_edges(node);
        let id = node.get("id").clone();
        let mut state = vec![
            (PyValue::Str("id".to_string()), id.clone()),
            (
                PyValue::Str("entry".to_string()),
                PyValue::Bool(edges.is_empty()),
            ),
            (
                PyValue::Str("max_entries".to_string()),
                PyValue::Int(STATE_MAX_ENTRIES_DEFAULT),
            ),
        ];
        for key in [
            "subagent",
            "lifecycle",
            "budget_ms",
            "retries",
            "failure_policy",
            "inputs",
            "outputs",
            "foreach",
        ] {
            if let Some(value) = node.entry(key) {
                state.push((PyValue::Str(key.to_string()), value.clone()));
            }
        }
        states.push(PyValue::Dict(state));
        if !edges.is_empty() {
            let from = if edges.len() == 1 {
                PyValue::Str(edges[0].clone())
            } else {
                PyValue::List(edges.into_iter().map(PyValue::Str).collect())
            };
            transitions.push(dict(vec![("from", from), ("to", id)]));
        }
    }
    let mut machine = Vec::new();
    machine.push((PyValue::Str("states".to_string()), PyValue::List(states)));
    machine.push((
        PyValue::Str("transitions".to_string()),
        PyValue::List(transitions),
    ));
    if let Some(run) = run {
        machine.push((PyValue::Str("run".to_string()), run.clone()));
    }
    (Some(PyValue::Dict(machine)), Vec::new())
}

// ---------------------------------------------------------------------------
// Unified entry points.
// ---------------------------------------------------------------------------

/// Dry-run validation for a spec in either form: a spec carrying `states`
/// or `transitions` is machine form; anything else is dag form and compiles
/// first. A spec carrying both is rejected outright.
#[must_use]
pub fn validate_factory_spec(spec: &PyValue) -> Vec<String> {
    if !spec.is_dict() {
        return vec!["factory dag must be a JSON object".to_string()];
    }
    if is_machine_form(spec) && spec.has("nodes") {
        return vec!["pass either dag or machine form, not both".to_string()];
    }
    if is_machine_form(spec) {
        return validate_factory_machine(spec);
    }
    match compile_factory_dag(spec) {
        (Some(machine), errors) if errors.is_empty() => validate_factory_machine(&machine),
        (_, errors) => errors,
    }
}

/// `_canonicalize_machine`: defaults applied to a validated machine.
fn canonicalize_machine(machine: &PyValue) -> PyValue {
    let run_in = machine.entry("run").filter(|run| run.is_dict());
    let run_get = |key: &str| run_in.and_then(|run| run.entry(key));
    let run_policy = run_get("failure_policy")
        .cloned()
        .unwrap_or_else(|| PyValue::Str(RUN_FAILURE_POLICY_DEFAULT.to_string()));
    let states_count = machine.get("states").as_list().map_or(0, <[PyValue]>::len) as i128;
    let mut run = vec![
        ("failure_policy", run_policy.clone()),
        (
            "max_parallel",
            run_get("max_parallel")
                .cloned()
                .unwrap_or(PyValue::Int(RUN_MAX_PARALLEL_DEFAULT)),
        ),
        (
            "max_transitions",
            run_get("max_transitions").cloned().unwrap_or(PyValue::Int(
                (TRANSITIONS_PER_STATE_DEFAULT * states_count).min(MAX_TRANSITIONS_CAP),
            )),
        ),
        (
            "max_children",
            run_get("max_children")
                .cloned()
                .unwrap_or(PyValue::Int(RUN_MAX_CHILDREN_DEFAULT)),
        ),
    ];
    if let Some(budget) = run_get("budget_ms") {
        run.push(("budget_ms", budget.clone()));
    }
    let mut states_out = Vec::new();
    for state in machine.get("states").as_list().unwrap_or(&[]) {
        let default = |key: &str, fallback: PyValue| state.entry(key).cloned().unwrap_or(fallback);
        let mut row = vec![
            ("id", state.get("id").clone()),
            ("entry", PyValue::Bool(state.get("entry").truthy())),
            (
                "max_entries",
                default("max_entries", PyValue::Int(STATE_MAX_ENTRIES_DEFAULT)),
            ),
            (
                "lifecycle",
                default(
                    "lifecycle",
                    PyValue::Str(NODE_LIFECYCLE_DEFAULT.to_string()),
                ),
            ),
            (
                "retries",
                default("retries", PyValue::Int(NODE_RETRIES_DEFAULT)),
            ),
            (
                "failure_policy",
                default("failure_policy", run_policy.clone()),
            ),
            ("subagent", state.get("subagent").clone()),
        ];
        for key in ["budget_ms", "inputs", "outputs", "foreach"] {
            if let Some(value) = state.entry(key) {
                row.push((key, value.clone()));
            }
        }
        states_out.push(dict(row));
    }
    let mut transitions_out = Vec::new();
    for transition in machine.get("transitions").as_list().unwrap_or(&[]) {
        let mut row = vec![
            ("from", transition.get("from").clone()),
            ("to", transition.get("to").clone()),
            (
                "on",
                transition
                    .entry("on")
                    .cloned()
                    .unwrap_or_else(|| PyValue::Str(TRANSITION_ON_KINDS[0].to_string())),
            ),
        ];
        if let Some(when) = transition.entry("when") {
            row.push(("when", when.clone()));
        }
        transitions_out.push(dict(row));
    }
    dict(vec![
        ("run", dict(run)),
        ("states", PyValue::List(states_out)),
        ("transitions", PyValue::List(transitions_out)),
    ])
}

/// Validate a spec in either form and return the canonical MACHINE form
/// (`{"run": ..., "states": [...], "transitions": [...]}` with defaults
/// applied: run `failure_policy` `escalate`, `max_parallel` 8,
/// `max_transitions` 10 per state capped at 10000, `max_children` 10000;
/// state `entry` false, `max_entries` 1, `lifecycle` `task`, `retries` 0,
/// `failure_policy` from the run; transition `on` `settled`).
///
/// # Errors
///
/// Returns the joined error sentences (`"; "`) when the spec is invalid —
/// the kernel raises them as one `ValueError`.
pub fn canonicalize_factory_spec(spec: &PyValue) -> Result<PyValue, String> {
    let errors = validate_factory_spec(spec);
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    if is_machine_form(spec) {
        return Ok(canonicalize_machine(spec));
    }
    match compile_factory_dag(spec) {
        (Some(machine), _) => Ok(canonicalize_machine(&machine)),
        (None, errors) => Err(errors.join("; ")),
    }
}

/// Node ids in a dependency-respecting order (the effective edges:
/// `depends_on` plus every `inputs[].from` source). Stable: among ready
/// nodes, input order wins. A public helper for inspecting dag-form specs;
/// the machine form has no acyclicity requirement.
///
/// # Errors
///
/// Returns the `ValueError` sentence on a malformed list, a duplicate id,
/// an unknown dependency, or a cycle.
pub fn topological_order(nodes: &PyValue) -> Result<Vec<String>, String> {
    let Some(nodes) = nodes.as_list() else {
        return Err("nodes must be a list".to_string());
    };
    let mut index_of: HashMap<String, usize> = HashMap::new();
    let mut ids: Vec<String> = Vec::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        if !node.is_dict() {
            return Err(format!("nodes[{index}] must be an object"));
        }
        let Some(node_id) = node.get("id").as_str().filter(|id| !id.is_empty()) else {
            return Err(format!("nodes[{index}] requires a non-empty id"));
        };
        if index_of.contains_key(node_id) {
            return Err(format!("duplicate node id {}", py_str_repr(node_id)));
        }
        index_of.insert(node_id.to_string(), index);
        ids.push(node_id.to_string());
    }

    let mut deps: Vec<HashSet<String>> = Vec::with_capacity(nodes.len());
    let mut dep_order: Vec<Vec<String>> = Vec::with_capacity(nodes.len());
    for (node, node_id) in nodes.iter().zip(&ids) {
        let mut edges: HashSet<String> = HashSet::new();
        let mut ordered: Vec<String> = Vec::new();
        let mut add = |edge: &str, edges: &mut HashSet<String>| {
            if edges.insert(edge.to_string()) {
                ordered.push(edge.to_string());
            }
        };
        let depends_on = node.get("depends_on");
        if !depends_on.is_none() {
            let Some(list) = depends_on.as_list() else {
                return Err(format!(
                    "node {} depends_on must be a list of node ids",
                    py_str_repr(node_id)
                ));
            };
            for dep in list {
                let Some(dep) = dep.as_str().filter(|dep| !dep.is_empty()) else {
                    return Err(format!(
                        "node {} depends_on entries must be non-empty node id strings",
                        py_str_repr(node_id)
                    ));
                };
                add(dep, &mut edges);
            }
        }
        let inputs = node.get("inputs");
        if !inputs.is_none() {
            let Some(list) = inputs.as_list() else {
                return Err(format!(
                    "node {} inputs must be a list",
                    py_str_repr(node_id)
                ));
            };
            for input in list {
                if !input.is_dict() {
                    return Err(format!(
                        "node {} inputs entries must be objects",
                        py_str_repr(node_id)
                    ));
                }
                let Some(source) = input
                    .get("from")
                    .as_str()
                    .filter(|source| source.contains('.'))
                else {
                    return Err(format!(
                        "node {} inputs require a 'from' reference of the form '<node_id>.<output_name>'",
                        py_str_repr(node_id)
                    ));
                };
                add(partition_dot(source).0, &mut edges);
            }
        }
        deps.push(edges);
        dep_order.push(ordered);
    }

    // Python iterates the dependency *set*; with no hash-order guarantee
    // only the first unknown dependency's identity can differ, and both
    // implementations name some unknown dependency of the first node that
    // has one. The declared order is used here.
    for (node_id, ordered) in ids.iter().zip(&dep_order) {
        for dep in ordered {
            if !index_of.contains_key(dep) {
                return Err(format!(
                    "node {} depends on unknown node {}",
                    py_str_repr(node_id),
                    py_str_repr(dep)
                ));
            }
        }
    }

    let mut remaining: Vec<usize> = deps.iter().map(HashSet::len).collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
    for (index, ordered) in dep_order.iter().enumerate() {
        for dep in ordered {
            dependents[index_of[dep]].push(index);
        }
    }
    let mut ready: BinaryHeap<std::cmp::Reverse<usize>> = remaining
        .iter()
        .enumerate()
        .filter(|(_, count)| **count == 0)
        .map(|(index, _)| std::cmp::Reverse(index))
        .collect();
    let mut order = Vec::with_capacity(ids.len());
    while let Some(std::cmp::Reverse(current)) = ready.pop() {
        order.push(ids[current].clone());
        for &dependent in &dependents[current] {
            remaining[dependent] -= 1;
            if remaining[dependent] == 0 {
                ready.push(std::cmp::Reverse(dependent));
            }
        }
    }
    if order.len() != ids.len() {
        let mut stuck: Vec<&str> = ids
            .iter()
            .zip(&remaining)
            .filter(|(_, count)| **count > 0)
            .map(|(id, _)| id.as_str())
            .collect();
        stuck.sort_unstable();
        return Err(format!(
            "the factory graph contains a cycle involving nodes: {}",
            stuck.join(", ")
        ));
    }
    Ok(order)
}

#[cfg(test)]
mod tests;
