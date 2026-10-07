//! The spec-operation surface the kernel's validator functions call.
//!
//! One request shape serves both transports: the in-kernel blocking
//! `factory.spec` host request (harness writes run synchronously inside a
//! cell) and the out-of-kernel `prime-agent --prime-agent-factory-spec`
//! filter (the machine-library CLI runner and the runtime's unit tests have
//! no host to ask). Both hand the same JSON to [`run_spec_op`].

use serde_json::{json, Value};

use super::pyvalue::{decode_node_table, encode_node_table};
use super::spec::{
    canonicalize_factory_spec, compile_factory_dag, topological_order, validate_factory_machine,
    validate_factory_spec,
};

/// The hidden `prime-agent` flag that runs one spec operation as a filter
/// (one JSON request on stdin, one JSON reply on stdout).
pub const FACTORY_SPEC_FILTER_FLAG: &str = "--prime-agent-factory-spec";

/// The environment variable naming the host binary a host-less runtime
/// process runs [`FACTORY_SPEC_FILTER_FLAG`] through.
pub const HOST_BINARY_ENV: &str = "PRIME_AGENT_HOST_BINARY";

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

/// The filter mode behind [`FACTORY_SPEC_FILTER_FLAG`]: read one request
/// from `input`, write one reply line to `output`. A malformed request
/// answers `{"failure": <reason>}` so the caller always gets a line.
///
/// # Errors
///
/// Returns an error when reading the input or writing the reply fails.
pub fn run_spec_filter(
    input: &mut dyn std::io::Read,
    output: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let mut raw = String::new();
    input.read_to_string(&mut raw)?;
    let reply = serde_json::from_str::<Value>(&raw)
        .map_err(anyhow::Error::from)
        .and_then(|request| run_spec_op(&request))
        .unwrap_or_else(|error| json!({ "failure": format!("{error:#}") }));
    writeln!(output, "{reply}")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::factory::pyvalue::PyValue;

    fn table(value: &Value) -> Value {
        encode_node_table(&PyValue::from_json(value))
    }

    #[test]
    fn the_filter_answers_one_line_per_request() {
        let request = json!({
            "op": "validate_spec",
            "value": table(&json!({ "nodes": [] })),
        });
        let mut output = Vec::new();
        run_spec_filter(&mut request.to_string().as_bytes(), &mut output).expect("filter");
        let reply: Value = serde_json::from_slice(&output).expect("reply json");
        assert_eq!(
            reply,
            json!({ "errors": ["factory dag must declare between 1 and 1024 nodes, got 0"] })
        );

        let mut output = Vec::new();
        run_spec_filter(
            &mut "{\"op\": \"nope\", \"value\": {\"nodes\": [[\"n\"]], \"root\": 0}}".as_bytes(),
            &mut output,
        )
        .expect("filter");
        let reply: Value = serde_json::from_slice(&output).expect("reply json");
        assert_eq!(
            reply,
            json!({ "failure": "unknown factory.spec op \"nope\"" })
        );
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
