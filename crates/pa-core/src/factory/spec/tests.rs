//! Rust ports of the validator battery (`prime-agent-runtime/test/
//! test_factory.py`). The full Python battery also runs against this
//! implementation through the kernel client; these pin the rules and the
//! Python-semantics cases (opaque values, NaN, non-string keys, depth)
//! without a Python process.

use serde_json::{json, Value};

use super::*;

fn py(value: &Value) -> PyValue {
    PyValue::from_json(value)
}

fn spec_errors(value: &Value) -> Vec<String> {
    validate_factory_spec(&py(value))
}

fn machine_errors(value: &Value) -> Vec<String> {
    validate_factory_machine(&py(value))
}

fn node(id: &str) -> Value {
    json!({ "id": id, "subagent": "worker" })
}

fn with(mut base: Value, extra: &Value) -> Value {
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    base
}

const FINITE_ERROR: &str = "transitions[0] when.value must be finite JSON data (JSON carries no NaN or Infinity, and only JSON shapes serialize: lists, objects, strings, numbers, booleans, null, and no container nests deeper than 256 levels)";

/// A machine whose single guard compares `value` with `op`.
fn guarded(op: &str, value: PyValue) -> PyValue {
    let mut machine = py(&json!({
        "states": [
            { "id": "a", "entry": true, "subagent": "w", "outputs": [{ "name": "verdict", "type": "json" }] },
            { "id": "b", "subagent": "w" }
        ],
        "transitions": [{ "from": "a", "to": "b", "when": { "output": "verdict", "op": op } }]
    }));
    if let PyValue::Dict(pairs) = &mut machine {
        if let Some((_, PyValue::List(transitions))) = pairs
            .iter_mut()
            .find(|(k, _)| k.as_str() == Some("transitions"))
        {
            if let Some(PyValue::Dict(transition)) = transitions.first_mut() {
                if let Some((_, PyValue::Dict(when))) = transition
                    .iter_mut()
                    .find(|(k, _)| k.as_str() == Some("when"))
                {
                    when.push((PyValue::Str("value".into()), value));
                }
            }
        }
    }
    machine
}

fn opaque(repr: &str) -> PyValue {
    PyValue::Opaque {
        index: 0,
        repr: repr.to_string(),
        truthy: true,
        type_name: "object".to_string(),
        json: None,
    }
}

#[test]
fn dag_must_be_an_object() {
    for bad in [json!(null), json!([]), json!("nodes"), json!(42)] {
        assert_eq!(
            spec_errors(&bad),
            ["factory dag must be a JSON object"],
            "{bad}"
        );
    }
}

#[test]
fn nodes_required_and_must_be_a_list() {
    assert_eq!(
        spec_errors(&json!({ "nodes": "nope" })),
        ["factory dag requires a nodes list"]
    );
    assert_eq!(
        spec_errors(&json!({ "run": "bad", "nodes": "nope" })),
        ["run must be an object", "factory dag requires a nodes list"]
    );
}

#[test]
fn node_cap_and_ids() {
    let at_cap: Vec<Value> = (0..1024).map(|i| node(&format!("n{i}"))).collect();
    assert_eq!(
        spec_errors(&json!({ "nodes": at_cap })),
        Vec::<String>::new()
    );
    let over_cap: Vec<Value> = (0..1025).map(|i| node(&format!("n{i}"))).collect();
    assert_eq!(
        spec_errors(&json!({ "nodes": over_cap })),
        ["factory dag must declare between 1 and 1024 nodes, got 1025"]
    );
    for good in ["a", "node-1", "1st-node", &"a".repeat(64)] {
        assert_eq!(
            spec_errors(&json!({ "nodes": [node(good)] })),
            Vec::<String>::new()
        );
    }
    assert_eq!(
        spec_errors(&json!({ "nodes": [{ "id": "ABC", "subagent": "w" }] })),
        ["nodes[0] id must match ^[a-z0-9][a-z0-9-]{0,63}$, got 'ABC'"]
    );
    for bad in [json!(""), json!(null), json!(5)] {
        assert_eq!(
            spec_errors(&json!({ "nodes": [{ "id": bad, "subagent": "w" }] })),
            ["nodes[0] requires a non-empty id"]
        );
    }
    assert_eq!(
        spec_errors(&json!({ "nodes": [node("dup"), node("dup")] })),
        ["nodes[1] duplicates node id 'dup'"]
    );
}

#[test]
fn subagent_forms() {
    let cases = [
        (json!({ "id": "a" }), "node a requires a subagent: a harness subagent id/title string or an inline object with a prompt"),
        (json!({ "id": "a", "subagent": { "prompt": "  \t " } }), "node a inline subagent requires a non-empty prompt"),
        (json!({ "id": "a", "subagent": { "prompt": "p", "model": 5 } }), "node a inline subagent model must be a non-empty string when provided"),
        (json!({ "id": "a", "subagent": { "prompt": "p", "name": "  " } }), "node a inline subagent name must be a non-empty string when provided"),
    ];
    for (bad, expected) in cases {
        assert_eq!(spec_errors(&json!({ "nodes": [bad] })), [expected]);
    }
    let long = "w".repeat(65);
    assert_eq!(
        spec_errors(
            &json!({ "nodes": [{ "id": "a", "subagent": { "prompt": "p", "name": long } }] })
        ),
        ["node a inline subagent name must be at most 64 characters, got 65"]
    );
    let duplicate = json!({ "nodes": [
        { "id": "a", "subagent": { "prompt": "p", "name": "w" } },
        { "id": "b", "subagent": { "prompt": "p", "name": "w" }, "depends_on": ["a"] }
    ] });
    assert_eq!(
        spec_errors(&duplicate),
        ["state b subagent name 'w' is already configured by state 'a'"]
    );
}

#[test]
fn suffixed_name_collisions_reject_both_directions() {
    let shadowing = json!({ "states": [
        { "id": "a", "entry": true, "subagent": { "prompt": "p", "name": "foo" } },
        { "id": "b", "entry": true, "subagent": { "prompt": "p", "name": "foo-i1" } }
    ] });
    assert_eq!(
        machine_errors(&shadowing),
        ["state b subagent name 'foo-i1' collides with the suffixed spawn labels of state 'a' (configured 'foo'): re-entry, foreach, and retries name children 'foo'-i<n> and 'foo'-a<n>"]
    );
    let reverse = json!({ "states": [
        { "id": "a", "entry": true, "subagent": { "prompt": "p", "name": "foo-a2" } },
        { "id": "b", "entry": true, "subagent": { "prompt": "p", "name": "foo" } }
    ] });
    assert_eq!(
        machine_errors(&reverse),
        ["state b subagent name 'foo' suffixed by re-entry, foreach, and retries ('foo'-i<n>, 'foo'-a<n>) collides with state 'a' (configured 'foo-a2')"]
    );
}

#[test]
fn typed_nulls_are_rejected_with_the_field_message() {
    assert_eq!(
        spec_errors(&json!({ "run": { "max_parallel": null }, "nodes": [node("a")] })),
        ["run max_parallel must be an integer between 1 and 64"]
    );
    assert_eq!(
        spec_errors(&json!({ "nodes": [with(node("a"), &json!({ "lifecycle": null }))] })),
        ["node a lifecycle must be 'task' or 'resident', got None"]
    );
    assert_eq!(
        machine_errors(
            &json!({ "states": [{ "id": "a", "entry": true, "subagent": "w", "max_entries": null }] })
        ),
        ["state a max_entries must be an integer >= 1"]
    );
    assert_eq!(
        spec_errors(&json!({ "run": { "failure_policy": "bogus" }, "nodes": [node("a")] })),
        ["run failure_policy must be one of ['fail_fast', 'continue', 'escalate'], got 'bogus'"]
    );
}

#[test]
fn booleans_are_not_integers() {
    assert_eq!(
        spec_errors(&json!({ "nodes": [with(node("a"), &json!({ "retries": true }))] })),
        ["node a retries must be an integer between 0 and 10"]
    );
    assert_eq!(
        spec_errors(&json!({ "run": { "budget_ms": 1.0 }, "nodes": [node("a")] })),
        ["run budget_ms must be a positive integer"]
    );
}

#[test]
fn budgets_and_ports() {
    assert_eq!(
        spec_errors(
            &json!({ "run": { "budget_ms": 100 }, "nodes": [with(node("a"), &json!({ "budget_ms": 200 }))] })
        ),
        ["node a budget_ms 200 exceeds the run budget_ms 100"]
    );
    let typed = json!({ "nodes": [
        with(node("a"), &json!({ "outputs": [{ "name": "out", "type": "text" }] })),
        with(node("b"), &json!({ "inputs": [{ "name": "in", "type": "json", "from": "a.out" }] }))
    ] });
    assert_eq!(
        spec_errors(&typed),
        ["node b input 'in' of type 'json' cannot read from output 'out' of type 'text'"]
    );
    let missing = json!({ "nodes": [
        with(node("a"), &json!({ "outputs": [{ "name": "out", "type": "text" }] })),
        with(node("b"), &json!({ "inputs": [{ "name": "in", "type": "text", "from": "a.missing" }] }))
    ] });
    assert_eq!(
        spec_errors(&missing),
        ["node b input 'in' references output 'missing' that node 'a' does not declare"]
    );
    let self_read = json!({ "nodes": [with(node("a"), &json!({
        "outputs": [{ "name": "o", "type": "text" }],
        "inputs": [{ "name": "i", "type": "text", "from": "a.o" }]
    }))] });
    assert_eq!(spec_errors(&self_read), ["node a cannot depend on itself"]);
}

#[test]
fn many_ports_validate_in_linear_time() {
    let mut outputs: Vec<Value> = (0..8000)
        .map(|i| json!({ "name": format!("o{i}"), "type": "text" }))
        .collect();
    outputs.push(json!({ "name": "o1", "type": "json" }));
    let started = std::time::Instant::now();
    let errors =
        spec_errors(&json!({ "nodes": [with(node("a"), &json!({ "outputs": outputs }))] }));
    assert_eq!(errors, ["node a declares duplicate output name 'o1'"]);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    let repeated: Vec<Value> = (0..500)
        .map(|_| json!({ "name": "dup", "type": "text" }))
        .collect();
    assert_eq!(
        spec_errors(&json!({ "nodes": [with(node("a"), &json!({ "outputs": repeated }))] })),
        ["node a declares duplicate output name 'dup'"]
    );
}

#[test]
fn both_forms_and_form_detection() {
    assert_eq!(
        spec_errors(&json!({ "nodes": [node("a")], "states": [] })),
        ["pass either dag or machine form, not both"]
    );
    assert_eq!(
        spec_errors(&json!({ "transitions": [] })),
        ["factory machine requires a states list"]
    );
}

#[test]
fn fully_cyclic_dag_has_no_entry_state() {
    let cycle = json!({ "nodes": [
        with(node("a"), &json!({ "depends_on": ["b"] })),
        with(node("b"), &json!({ "depends_on": ["a"] }))
    ] });
    assert_eq!(
        spec_errors(&cycle),
        ["factory machine requires at least one entry state"]
    );
}

#[test]
fn compile_emits_one_join_per_fan_in_node() {
    let dag = json!({
        "run": { "max_parallel": 2 },
        "nodes": [
            node("a"),
            node("b"),
            with(node("c"), &json!({ "depends_on": ["a", "b", "a"] }))
        ]
    });
    let (machine, errors) = compile_factory_dag(&py(&dag));
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(
        machine.map(|machine| machine.to_json()),
        Some(json!({
            "states": [
                { "id": "a", "entry": true, "max_entries": 1, "subagent": "worker" },
                { "id": "b", "entry": true, "max_entries": 1, "subagent": "worker" },
                { "id": "c", "entry": false, "max_entries": 1, "subagent": "worker" }
            ],
            "transitions": [{ "from": ["a", "b"], "to": "c" }],
            "run": { "max_parallel": 2 }
        }))
    );
    let (machine, errors) = compile_factory_dag(&py(
        &json!({ "nodes": [with(node("a"), &json!({ "depends_on": ["ghost"] }))] }),
    ));
    assert_eq!(
        (machine, errors),
        (
            None,
            vec!["node a depends on unknown node 'ghost'".to_string()]
        )
    );
}

#[test]
fn canonicalize_applies_defaults_and_joins_errors() {
    let canonical = canonicalize_factory_spec(&py(
        &json!({ "run": { "failure_policy": "continue" }, "states": [
        { "id": "a", "entry": true, "subagent": "w", "outputs": [{ "name": "o", "type": "text" }] },
        { "id": "b", "subagent": { "prompt": "p" }, "retries": 2 }
    ], "transitions": [{ "from": "a", "to": "b" }] }),
    ))
    .expect("valid");
    assert_eq!(
        canonical.to_json(),
        json!({
            "run": { "failure_policy": "continue", "max_parallel": 8, "max_transitions": 20, "max_children": 10000 },
            "states": [
                { "id": "a", "entry": true, "max_entries": 1, "lifecycle": "task", "retries": 0, "failure_policy": "continue", "subagent": "w", "outputs": [{ "name": "o", "type": "text" }] },
                { "id": "b", "entry": false, "max_entries": 1, "lifecycle": "task", "retries": 2, "failure_policy": "continue", "subagent": { "prompt": "p" } }
            ],
            "transitions": [{ "from": "a", "to": "b", "on": "settled" }]
        })
    );
    assert_eq!(
        canonicalize_factory_spec(&py(&json!({ "nodes": [] }))),
        Err("factory dag must declare between 1 and 1024 nodes, got 0".to_string())
    );
}

#[test]
fn guard_rules() {
    let cases = [
        (guarded("eq", PyValue::List(vec![])), "transitions[0] when.op 'eq' requires a scalar value"),
        (guarded("gt", PyValue::Str("x".into())), "transitions[0] when.op 'gt' requires a numeric value"),
        (guarded("contains", PyValue::List(vec![])), "transitions[0] when.op 'contains' requires a non-empty list value"),
        (guarded("bogus", PyValue::None), "transitions[0] when.op must be one of ['eq', 'ne', 'gt', 'gte', 'lt', 'lte', 'exists', 'contains'], got 'bogus'"),
    ];
    for (machine, expected) in cases {
        assert_eq!(validate_factory_machine(&machine), [expected]);
    }
    assert_eq!(
        validate_factory_machine(&guarded("exists", PyValue::None)),
        Vec::<String>::new()
    );
    let join_guard = json!({ "states": [
        { "id": "a", "entry": true, "subagent": "w" },
        { "id": "b", "entry": true, "subagent": "w" },
        { "id": "c", "subagent": "w" }
    ], "transitions": [{ "from": ["a", "b"], "to": "c", "when": { "output": "x", "op": "exists" } }] });
    assert_eq!(
        machine_errors(&join_guard),
        ["transitions[0] with multiple from-states cannot carry a when guard; use single-state transitions for guards"]
    );
}

#[test]
fn guard_values_must_be_finite_json_data() {
    let bad_values = [
        PyValue::Float(f64::NAN),
        PyValue::Float(f64::INFINITY),
        PyValue::Float(f64::NEG_INFINITY),
    ];
    for value in bad_values {
        assert_eq!(
            validate_factory_machine(&guarded("eq", value)),
            [FINITE_ERROR]
        );
    }
    let nested = PyValue::List(vec![
        PyValue::Str("ok".into()),
        PyValue::Dict(vec![(PyValue::Str("x".into()), PyValue::Float(f64::NAN))]),
    ]);
    assert_eq!(
        validate_factory_machine(&guarded("contains", nested)),
        [FINITE_ERROR]
    );
    // Non-string keys, opaque leaves (a tuple, a set, bytes, a cycle's
    // back-reference): none of them is JSON data.
    for bad in [
        PyValue::Dict(vec![(PyValue::Int(1), PyValue::Str("x".into()))]),
        PyValue::Dict(vec![(PyValue::Float(f64::NAN), PyValue::Int(1))]),
        opaque("('plain', 'tuple')"),
        opaque("{'set'}"),
        opaque("b'bytes'"),
        opaque("[...]"),
    ] {
        let value = PyValue::List(vec![PyValue::Str("ok".into()), bad]);
        assert_eq!(
            validate_factory_machine(&guarded("contains", value)),
            [FINITE_ERROR]
        );
    }
    let fine = py(&json!(["ok", { "flag": true, "nested": { "count": 2 } }]));
    assert_eq!(
        validate_factory_machine(&guarded("contains", fine)),
        Vec::<String>::new()
    );
}

#[test]
fn guard_value_depth_is_bounded() {
    let mut deep = PyValue::List(vec![]);
    for _ in 0..(MAX_GUARD_VALUE_DEPTH + 50) {
        deep = PyValue::List(vec![deep]);
    }
    assert_eq!(
        validate_factory_machine(&guarded("contains", deep)),
        [FINITE_ERROR]
    );
    let mut within = PyValue::List(vec![PyValue::Str("verdict".into())]);
    for _ in 0..10 {
        within = PyValue::List(vec![within]);
    }
    assert_eq!(
        validate_factory_machine(&guarded("contains", within)),
        Vec::<String>::new()
    );
}

#[test]
fn opaque_values_read_as_no_json_shape() {
    // A tuple where a list belongs is not a list: `states` reads absent.
    let machine = PyValue::Dict(vec![(
        PyValue::Str("states".into()),
        opaque("({'id': 'a'},)"),
    )]);
    assert_eq!(
        validate_factory_machine(&machine),
        ["factory machine requires a states list"]
    );
    let policy = PyValue::Dict(vec![
        (
            PyValue::Str("run".into()),
            PyValue::Dict(vec![(
                PyValue::Str("failure_policy".into()),
                opaque("('continue',)"),
            )]),
        ),
        (PyValue::Str("nodes".into()), py(&json!([node("a")]))),
    ]);
    assert_eq!(
        validate_factory_spec(&policy),
        ["run failure_policy must be one of ['fail_fast', 'continue', 'escalate'], got ('continue',)"]
    );
}

#[test]
fn self_inputs_must_be_optional() {
    let required = json!({ "states": [
        { "id": "a", "entry": true, "subagent": "w" },
        { "id": "loop", "subagent": "w", "max_entries": 3,
          "outputs": [{ "name": "o", "type": "json" }],
          "inputs": [{ "name": "prev", "type": "json", "from": "loop.o" }] }
    ], "transitions": [{ "from": "a", "to": "loop" }, { "from": "loop", "to": "loop" }] });
    assert_eq!(
        machine_errors(&required),
        ["state loop input 'prev' cannot require itself: mark the self-input optional - a required one can never bind on the state's first entry"]
    );
}

#[test]
fn transition_references_and_residents() {
    let machine = json!({ "states": [
        { "id": "a", "entry": true, "subagent": "w", "lifecycle": "resident" },
        { "id": "b", "subagent": "w" }
    ], "transitions": [
        { "from": "a", "to": "b" },
        { "from": "ghost", "to": "b" },
        { "from": "b", "to": "nowhere", "on": "done" },
        { "from": ["b", "b"], "to": "b" },
        "nope"
    ] });
    assert_eq!(
        machine_errors(&machine),
        [
            "transitions[0] cannot leave resident state 'a'",
            "transitions[1] references unknown from-state 'ghost'",
            "transitions[2] references unknown to-state 'nowhere'",
            "transitions[2] on must be one of ['settled'], got 'done'",
            "transitions[3] from must not repeat a state",
            "transitions[4] must be an object",
        ]
    );
}

#[test]
fn topological_order_is_stable_and_names_cycles() {
    let nodes = json!([
        with(node("c"), &json!({ "depends_on": ["a"] })),
        node("a"),
        with(
            node("b"),
            &json!({ "inputs": [{ "name": "i", "type": "text", "from": "a.o" }] })
        )
    ]);
    assert_eq!(
        topological_order(&py(&nodes)),
        Ok(vec!["a".to_string(), "c".to_string(), "b".to_string()])
    );
    let cycle = json!([
        with(node("x"), &json!({ "depends_on": ["y"] })),
        with(node("y"), &json!({ "depends_on": ["x"] })),
        node("z")
    ]);
    assert_eq!(
        topological_order(&py(&cycle)),
        Err("the factory graph contains a cycle involving nodes: x, y".to_string())
    );
    assert_eq!(
        topological_order(&py(&json!([node("a"), node("a")]))),
        Err("duplicate node id 'a'".to_string())
    );
}
