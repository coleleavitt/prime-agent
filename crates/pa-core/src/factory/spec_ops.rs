//! The spec-operation surface the kernel's validator functions call.
//!
//! One request shape serves both transports: the in-kernel blocking
//! `factory.spec` host request (harness writes run synchronously inside a
//! cell) and the out-of-kernel `prime-agent --prime-agent-harness-request`
//! one-shot (a runtime process with no serving kernel). Both hand the same
//! JSON to [`run_spec_op`].

use serde_json::{json, Value};

use super::pyvalue::{decode_node_table, encode_node_table};
use super::spec::{
    canonicalize_factory_spec, compile_factory_dag, topological_order, validate_factory_machine,
    validate_factory_spec,
};

/// Run one spec operation: `{"op": <name>, "value": <node table>}`.
///
/// Replies: `validate_spec`/`validate_machine` -> `{"errors": [...]}`;
/// `compile_dag` -> `{"machine": <table> | null, "errors": [...]}`;
/// `canonicalize` -> `{"machine": <table>}` or `{"error": <sentence>}`;
/// `topological_order` -> `{"order": [...]}` or `{"error": <sentence>}`.
/// Domain failures are data (the kernel raises them as `ValueError`).
///
/// # Errors
///
/// Returns an error for a malformed request (unknown op, bad node table).
pub fn run_spec_op(request: &Value) -> anyhow::Result<Value> {
    let op = request
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("factory.spec op must be a string"))?;
    let table = request
        .get("value")
        .ok_or_else(|| anyhow::anyhow!("factory.spec value is required"))?;
    let value = decode_node_table(table)?;
    Ok(match op {
        "validate_spec" => json!({ "errors": validate_factory_spec(&value) }),
        "validate_machine" => json!({ "errors": validate_factory_machine(&value) }),
        "compile_dag" => {
            let (machine, errors) = compile_factory_dag(&value);
            json!({
                "machine": machine.as_ref().map(encode_node_table),
                "errors": errors,
            })
        }
        "canonicalize" => match canonicalize_factory_spec(&value) {
            Ok(machine) => json!({ "machine": encode_node_table(&machine) }),
            Err(error) => json!({ "error": error }),
        },
        "topological_order" => match topological_order(&value) {
            Ok(order) => json!({ "order": order }),
            Err(error) => json!({ "error": error }),
        },
        other => anyhow::bail!("unknown factory.spec op {other:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::pyvalue::PyValue;

    fn table(value: &Value) -> Value {
        encode_node_table(&PyValue::from_json(value))
    }

    #[test]
    fn domain_failures_are_data_and_malformed_requests_are_errors() {
        let reply = run_spec_op(&json!({
            "op": "validate_spec",
            "value": table(&json!({ "nodes": [] })),
        }))
        .expect("op");
        assert_eq!(
            reply,
            json!({ "errors": ["factory dag must declare between 1 and 1024 nodes, got 0"] })
        );
        let error = run_spec_op(&json!({"op": "nope", "value": {"nodes": [["n"]], "root": 0}}))
            .expect_err("unknown op");
        assert_eq!(error.to_string(), "unknown factory.spec op \"nope\"");
    }

    #[test]
    fn canonicalize_round_trips_through_node_tables() {
        let reply = run_spec_op(&json!({
            "op": "canonicalize",
            "value": table(&json!({ "nodes": [{ "id": "a", "subagent": "w" }] })),
        }))
        .expect("op");
        let machine = decode_node_table(&reply["machine"]).expect("machine table");
        assert_eq!(
            machine.to_json(),
            json!({
                "run": {
                    "failure_policy": "escalate",
                    "max_parallel": 8,
                    "max_transitions": 10,
                    "max_children": 10000
                },
                "states": [{
                    "id": "a",
                    "entry": true,
                    "max_entries": 1,
                    "lifecycle": "task",
                    "retries": 0,
                    "failure_policy": "escalate",
                    "subagent": "w"
                }],
                "transitions": []
            })
        );
    }
}
